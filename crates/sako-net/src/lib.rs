// SPDX-License-Identifier: BSD-3-Clause

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::windows::io::{AsRawSocket, AsSocket, BorrowedSocket};
use std::time::Duration;

use sako_platform::{IocpReactor, PostError};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ConnectionId {
    index: u32,
    generation: u32,
}

impl ConnectionId {
    pub fn index(self) -> usize {
        self.index as usize
    }

    pub fn completion_key(self) -> usize {
        ((self.generation as u64) << 32 | self.index as u64) as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcpServerConfig {
    pub maximum_connections: usize,
    pub maximum_accepts_per_tick: usize,
}

impl Default for TcpServerConfig {
    fn default() -> Self {
        Self {
            maximum_connections: 1024,
            maximum_accepts_per_tick: 64,
        }
    }
}

struct TcpConnection {
    stream: TcpStream,
    peer: SocketAddr,
    pending_read: Option<u64>,
    pending_write: Option<u64>,
    ready_read: Vec<u8>,
    ready_read_offset: usize,
    completed_write: Option<usize>,
    read_closed: bool,
    failed: bool,
    closing: bool,
}

/// Transport work counters. Each field counts an operation the acceptor
/// already performs, so keeping them costs one increment on a path that just
/// made a system call.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TcpAcceptorCounters {
    pub receives_submitted: u64,
    pub sends_submitted: u64,
    pub completions: u64,
    pub completion_dequeues: u64,
}

pub struct TcpAcceptor {
    // The reactor is declared first so it is dropped first: draining its
    // in-flight operations requires the connection sockets to still be open.
    reactor: IocpReactor,
    listener: TcpListener,
    connections: ConnectionSlab<TcpConnection>,
    operation_owners: HashMap<u64, ConnectionId>,
    completed: Vec<u64>,
    maximum_accepts_per_tick: usize,
    rejected_connections: u64,
    accepting: bool,
    counters: TcpAcceptorCounters,
}

impl TcpAcceptor {
    pub fn bind(address: impl ToSocketAddrs, config: TcpServerConfig) -> io::Result<Self> {
        if config.maximum_accepts_per_tick == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "maximum accepts per tick must be positive",
            ));
        }
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        let connections = ConnectionSlab::with_capacity(config.maximum_connections)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let operation_capacity = config
            .maximum_connections
            .checked_mul(2)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "I/O capacity overflow"))?;
        let reactor = IocpReactor::new(operation_capacity)?;
        Ok(Self {
            reactor,
            listener,
            connections,
            operation_owners: HashMap::with_capacity(operation_capacity),
            completed: Vec::with_capacity(operation_capacity.min(256)),
            maximum_accepts_per_tick: config.maximum_accepts_per_tick,
            rejected_connections: 0,
            accepting: true,
            counters: TcpAcceptorCounters::default(),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub fn accept_ready(&mut self) -> io::Result<Vec<ConnectionId>> {
        if !self.accepting {
            return Ok(Vec::new());
        }
        let mut accepted = Vec::with_capacity(
            self.maximum_accepts_per_tick
                .min(self.connections.capacity()),
        );
        for _ in 0..self.maximum_accepts_per_tick {
            let (stream, peer) = match self.listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            };
            stream.set_nonblocking(true)?;
            let id = match self.connections.insert(TcpConnection {
                stream,
                peer,
                pending_read: None,
                pending_write: None,
                ready_read: Vec::new(),
                ready_read_offset: 0,
                completed_write: None,
                read_closed: false,
                failed: false,
                closing: false,
            }) {
                Ok(id) => id,
                Err(_) => {
                    self.rejected_connections = self.rejected_connections.saturating_add(1);
                    continue;
                }
            };
            let socket = self.connections.get(id).unwrap().stream.as_socket();
            if let Err(error) = self.reactor.associate_socket(socket, id.completion_key()) {
                self.connections.remove(id);
                return Err(error);
            }
            accepted.push(id);
        }
        Ok(accepted)
    }

    pub fn stop_accepting(&mut self) {
        self.accepting = false;
    }

    pub fn connection_count(&self) -> usize {
        self.connections
            .values()
            .filter(|connection| !connection.closing)
            .count()
    }

    pub fn rejected_connections(&self) -> u64 {
        self.rejected_connections
    }

    pub fn counters(&self) -> TcpAcceptorCounters {
        self.counters
    }

    pub fn peer_addr(&self, id: ConnectionId) -> Option<SocketAddr> {
        self.connections.get(id).map(|connection| connection.peer)
    }

    pub fn read(&mut self, id: ConnectionId, buffer: &mut [u8]) -> io::Result<usize> {
        let connection = self.live_connection_mut(id)?;
        if connection.failed {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "overlapped receive failed",
            ));
        }
        if connection.ready_read_offset < connection.ready_read.len() {
            let available = &connection.ready_read[connection.ready_read_offset..];
            let length = available.len().min(buffer.len());
            buffer[..length].copy_from_slice(&available[..length]);
            connection.ready_read_offset += length;
            self.release_consumed_read(id);
            return Ok(length);
        }
        if connection.read_closed || buffer.is_empty() {
            return Ok(0);
        }
        self.submit_receive(id, buffer.len())?;
        Err(io::ErrorKind::WouldBlock.into())
    }

    /// Appends up to `limit` received bytes to `output` without staging them in
    /// a caller-owned scratch buffer, submitting a `chunk`-sized receive when no
    /// buffered bytes remain.
    pub fn read_into(
        &mut self,
        id: ConnectionId,
        output: &mut Vec<u8>,
        limit: usize,
        chunk: usize,
    ) -> io::Result<usize> {
        let connection = self.live_connection_mut(id)?;
        if connection.failed {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "overlapped receive failed",
            ));
        }
        if connection.ready_read_offset < connection.ready_read.len() {
            let available = &connection.ready_read[connection.ready_read_offset..];
            let length = available.len().min(limit);
            output.extend_from_slice(&available[..length]);
            connection.ready_read_offset += length;
            self.release_consumed_read(id);
            return Ok(length);
        }
        if connection.read_closed {
            return Ok(0);
        }
        if limit == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.submit_receive(id, chunk)?;
        Err(io::ErrorKind::WouldBlock.into())
    }

    fn release_consumed_read(&mut self, id: ConnectionId) {
        let Some(connection) = self.connections.get_mut(id) else {
            return;
        };
        if connection.ready_read_offset != connection.ready_read.len() {
            return;
        }
        let buffer = std::mem::take(&mut connection.ready_read);
        connection.ready_read_offset = 0;
        self.reactor.recycle_buffer(buffer);
    }

    fn submit_receive(&mut self, id: ConnectionId, length: usize) -> io::Result<()> {
        let connection = self.live_connection_mut(id)?;
        if connection.pending_read.is_some() {
            return Ok(());
        }
        let socket = connection.stream.as_raw_socket();
        let length = u32::try_from(length).unwrap_or(u32::MAX);
        // SAFETY: accept_ready associated this connection's socket file object
        // with this reactor, and the connection owns that socket until every
        // pending operation on it has been drained by drain_completions.
        let operation = unsafe {
            self.reactor.submit_associated_socket_receive(
                BorrowedSocket::borrow_raw(socket),
                id.completion_key(),
                length,
            )
        }
        .map_err(post_error)?;
        self.connections.get_mut(id).unwrap().pending_read = Some(operation);
        self.operation_owners.insert(operation, id);
        self.counters.receives_submitted += 1;
        Ok(())
    }

    pub fn write(&mut self, id: ConnectionId, buffer: &[u8]) -> io::Result<usize> {
        let connection = self.live_connection_mut(id)?;
        if connection.failed {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "overlapped send failed",
            ));
        }
        if let Some(transferred) = connection.completed_write.take() {
            return Ok(transferred);
        }
        if buffer.is_empty() {
            return Ok(0);
        }
        if connection.pending_write.is_some() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let socket = connection.stream.as_raw_socket();
        let mut payload = self.reactor.acquire_buffer(buffer.len());
        payload.copy_from_slice(buffer);
        // SAFETY: accept_ready associated this connection's socket file object
        // with this reactor, and the connection owns that socket until every
        // pending operation on it has been drained by drain_completions.
        let operation = unsafe {
            self.reactor.submit_associated_socket_send(
                BorrowedSocket::borrow_raw(socket),
                id.completion_key(),
                payload,
            )
        }
        .map_err(post_error)?;
        self.connections.get_mut(id).unwrap().pending_write = Some(operation);
        self.operation_owners.insert(operation, id);
        self.counters.sends_submitted += 1;
        Err(io::ErrorKind::WouldBlock.into())
    }

    pub fn close(&mut self, id: ConnectionId) -> bool {
        let Some(connection) = self.connections.get_mut(id) else {
            return false;
        };
        connection.closing = true;
        let operations = [connection.pending_read, connection.pending_write];
        for operation in operations.into_iter().flatten() {
            let _ = self.reactor.cancel_operation(operation);
        }
        if operations.iter().all(Option::is_none) {
            self.connections.remove(id);
        }
        true
    }

    fn live_connection_mut(&mut self, id: ConnectionId) -> io::Result<&mut TcpConnection> {
        self.connections
            .get_mut(id)
            .filter(|connection| !connection.closing)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connection is not live"))
    }

    /// Waits up to `timeout` for socket completions and applies them to their
    /// connections. Callers drive this once per event-loop turn; the read and
    /// write paths never poll on their own.
    pub fn poll_io(&mut self, timeout: Duration) -> io::Result<usize> {
        self.drain_completions(timeout)
    }

    fn drain_completions(&mut self, timeout: Duration) -> io::Result<usize> {
        let mut completed = std::mem::take(&mut self.completed);
        completed.clear();
        let result = self
            .reactor
            .poll_operations(timeout, self.reactor.capacity(), &mut completed);
        if !completed.is_empty() {
            self.counters.completion_dequeues += 1;
            self.counters.completions += completed.len() as u64;
        }
        let mut applied = 0;
        for operation in completed.drain(..) {
            let Some(id) = self.operation_owners.remove(&operation) else {
                continue;
            };
            let Some(mut completion) = self.reactor.take_operation_completion(operation) else {
                continue;
            };
            let Some(connection) = self.connections.get_mut(id) else {
                self.reactor.recycle_buffer(completion.buffer);
                continue;
            };
            if connection.pending_read == Some(operation) {
                connection.pending_read = None;
                if completion.succeeded() {
                    completion
                        .buffer
                        .truncate(completion.bytes_transferred as usize);
                    if completion.buffer.is_empty() {
                        connection.read_closed = true;
                        self.reactor.recycle_buffer(completion.buffer);
                    } else {
                        let stale =
                            std::mem::replace(&mut connection.ready_read, completion.buffer);
                        connection.ready_read_offset = 0;
                        self.reactor.recycle_buffer(stale);
                    }
                } else {
                    if !completion.cancelled() {
                        connection.failed = true;
                    }
                    self.reactor.recycle_buffer(completion.buffer);
                }
            } else if connection.pending_write == Some(operation) {
                connection.pending_write = None;
                if completion.succeeded() {
                    connection.completed_write = Some(completion.bytes_transferred as usize);
                } else if !completion.cancelled() {
                    connection.failed = true;
                }
                self.reactor.recycle_buffer(completion.buffer);
            } else {
                self.reactor.recycle_buffer(completion.buffer);
            }
            applied += 1;
            if self.connections.get(id).is_some_and(|connection| {
                connection.closing
                    && connection.pending_read.is_none()
                    && connection.pending_write.is_none()
            }) {
                self.connections.remove(id);
            }
        }
        self.completed = completed;
        result?;
        Ok(applied)
    }
}

fn post_error(error: PostError) -> io::Error {
    match error {
        PostError::Full => io::ErrorKind::WouldBlock.into(),
        PostError::System(error) => error,
    }
}

pub fn resolve_host(host: &str, port: u16, maximum_results: usize) -> io::Result<Vec<SocketAddr>> {
    if maximum_results == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "maximum DNS results must be positive",
        ));
    }
    (host, port)
        .to_socket_addrs()
        .map(|addresses| addresses.take(maximum_results).collect::<Vec<SocketAddr>>())
}

struct Slot<T> {
    generation: u32,
    value: Option<T>,
}

pub struct ConnectionSlab<T> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
    live: usize,
}

impl<T> ConnectionSlab<T> {
    pub fn with_capacity(capacity: usize) -> Result<Self, CapacityError> {
        if capacity == 0 || capacity > u32::MAX as usize {
            return Err(CapacityError);
        }
        let slots = (0..capacity)
            .map(|_| Slot {
                generation: 0,
                value: None,
            })
            .collect();
        let free = (0..capacity as u32).rev().collect();
        Ok(Self {
            slots,
            free,
            live: 0,
        })
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    fn values(&self) -> impl Iterator<Item = &T> {
        self.slots.iter().filter_map(|slot| slot.value.as_ref())
    }

    pub fn insert(&mut self, value: T) -> Result<ConnectionId, CapacityError> {
        let index = self.free.pop().ok_or(CapacityError)?;
        let slot = &mut self.slots[index as usize];
        debug_assert!(slot.value.is_none());
        slot.value = Some(value);
        self.live += 1;
        Ok(ConnectionId {
            index,
            generation: slot.generation,
        })
    }

    pub fn get(&self, id: ConnectionId) -> Option<&T> {
        self.slots
            .get(id.index())
            .filter(|slot| slot.generation == id.generation)
            .and_then(|slot| slot.value.as_ref())
    }

    pub fn get_mut(&mut self, id: ConnectionId) -> Option<&mut T> {
        self.slots
            .get_mut(id.index())
            .filter(|slot| slot.generation == id.generation)
            .and_then(|slot| slot.value.as_mut())
    }

    pub fn remove(&mut self, id: ConnectionId) -> Option<T> {
        let slot = self.slots.get_mut(id.index())?;
        if slot.generation != id.generation {
            return None;
        }
        let value = slot.value.take()?;
        slot.generation = slot.generation.wrapping_add(1);
        self.free.push(id.index);
        self.live -= 1;
        Some(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacityError;

impl fmt::Display for CapacityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded capacity is exhausted")
    }
}

impl std::error::Error for CapacityError {}

pub struct BufferPool {
    buffers: Vec<Vec<u8>>,
    buffer_capacity: usize,
    maximum_buffers: usize,
}

impl BufferPool {
    pub fn new(maximum_buffers: usize, buffer_capacity: usize) -> Result<Self, CapacityError> {
        if maximum_buffers == 0 || buffer_capacity == 0 {
            return Err(CapacityError);
        }
        let mut buffers = Vec::with_capacity(maximum_buffers);
        for _ in 0..maximum_buffers {
            buffers.push(Vec::with_capacity(buffer_capacity));
        }
        Ok(Self {
            buffers,
            buffer_capacity,
            maximum_buffers,
        })
    }

    pub fn take(&mut self) -> Option<Vec<u8>> {
        self.buffers.pop()
    }

    pub fn put(&mut self, mut buffer: Vec<u8>) -> Result<(), Vec<u8>> {
        if self.buffers.len() >= self.maximum_buffers || buffer.capacity() != self.buffer_capacity {
            return Err(buffer);
        }
        buffer.clear();
        self.buffers.push(buffer);
        Ok(())
    }

    pub fn available(&self) -> usize {
        self.buffers.len()
    }

    pub fn reserved_bytes(&self) -> usize {
        self.buffers.len().saturating_mul(self.buffer_capacity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn stale_connection_ids_cannot_access_reused_slots() {
        let mut slab = ConnectionSlab::with_capacity(1).unwrap();
        let first = slab.insert("first").unwrap();
        assert!(slab.insert("overflow").is_err());
        assert_eq!(slab.remove(first), Some("first"));
        let second = slab.insert("second").unwrap();
        assert_ne!(first, second);
        assert!(slab.get(first).is_none());
        assert_eq!(slab.get(second), Some(&"second"));
    }

    #[test]
    fn buffer_pool_never_grows_past_its_configuration() {
        let mut pool = BufferPool::new(2, 4096).unwrap();
        let first = pool.take().unwrap();
        let second = pool.take().unwrap();
        assert!(pool.take().is_none());
        pool.put(first).unwrap();
        pool.put(second).unwrap();
        assert_eq!(pool.available(), 2);
        assert_eq!(pool.reserved_bytes(), 8192);
        assert!(pool.put(Vec::with_capacity(4096)).is_err());
    }

    #[test]
    fn tcp_acceptance_is_bounded_and_connections_are_owned() {
        let mut acceptor = TcpAcceptor::bind(
            "127.0.0.1:0",
            TcpServerConfig {
                maximum_connections: 1,
                maximum_accepts_per_tick: 2,
            },
        )
        .unwrap();
        let address = acceptor.local_addr().unwrap();
        let mut first = TcpStream::connect(address).unwrap();
        first.write_all(b"ping").unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let accepted = loop {
            let accepted = acceptor.accept_ready().unwrap();
            if !accepted.is_empty() {
                break accepted;
            }
            assert!(Instant::now() < deadline, "accept timed out");
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(accepted.len(), 1);
        assert_eq!(acceptor.connection_count(), 1);
        let _second = TcpStream::connect(address).unwrap();
        while acceptor.rejected_connections() == 0 {
            acceptor.accept_ready().unwrap();
            assert!(Instant::now() < deadline, "overload rejection timed out");
            thread::sleep(Duration::from_millis(1));
        }
        let id = accepted[0];
        let mut buffer = [0_u8; 4];
        loop {
            match acceptor.read(id, &mut buffer) {
                Ok(4) => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    acceptor.poll_io(Duration::from_millis(1)).unwrap();
                }
                result => panic!("unexpected read result: {result:?}"),
            }
        }
        assert_eq!(&buffer, b"ping");
        assert!(acceptor.peer_addr(id).is_some());
        assert!(acceptor.close(id));
        assert_eq!(acceptor.connection_count(), 0);
    }

    #[test]
    fn dns_results_are_bounded() {
        let addresses = resolve_host("localhost", 80, 1).unwrap();
        assert!(addresses.len() <= 1);
    }

    #[test]
    fn closing_a_connection_cancels_and_drains_pending_receive() {
        let mut acceptor = TcpAcceptor::bind(
            "127.0.0.1:0",
            TcpServerConfig {
                maximum_connections: 1,
                maximum_accepts_per_tick: 1,
            },
        )
        .unwrap();
        let _client = TcpStream::connect(acceptor.local_addr().unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let id = loop {
            if let Some(id) = acceptor.accept_ready().unwrap().into_iter().next() {
                break id;
            }
            assert!(Instant::now() < deadline, "accept timed out");
            thread::sleep(Duration::from_millis(1));
        };
        let mut buffer = [0_u8; 16];
        assert_eq!(
            acceptor.read(id, &mut buffer).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(acceptor.reactor.pending_operations(), 1);
        assert!(acceptor.close(id));
        assert_eq!(acceptor.connection_count(), 0);
        while !acceptor.connections.is_empty() {
            acceptor.drain_completions(Duration::ZERO).unwrap();
            assert!(Instant::now() < deadline, "cancel drain timed out");
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(acceptor.reactor.pending_operations(), 0);
        assert_eq!(acceptor.reactor.retained_completions(), 0);
    }
}
