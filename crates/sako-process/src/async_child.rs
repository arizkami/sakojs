// SPDX-License-Identifier: BSD-3-Clause

//! A running child whose streams stay open.
//!
//! [`crate::spawn_native_with_bounded_output`] answers one question -- what
//! did this command print before it exited -- and answers it by waiting. A
//! long-lived child answers a different one: a build service, a language
//! server, or a REPL is spoken to over stdin and replies on stdout while it
//! keeps running, and the caller has to see those replies as they arrive.
//!
//! Three threads per child do that here: one draining stdout, one draining
//! stderr, and one feeding stdin. They hand what they read to a shared queue
//! the event loop drains on its own turn, so no JavaScript frame ever blocks
//! on a pipe. A process-wide condition variable tells that loop when a queue
//! has something in it, which is what lets it block instead of polling.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

/// How much child output may sit undelivered before the reader threads stop
/// draining the pipe.
///
/// Reaching it is backpressure, not an error: the reader stops, the pipe
/// fills, and the child's next write blocks until the consumer catches up. A
/// cap that failed instead would turn a slow consumer into a dead child.
pub const MAXIMUM_CHILD_BUFFERED_BYTES: usize = 8 * 1024 * 1024;

/// How many bytes a single read asks for. Large enough that a build tool's
/// multi-megabyte reply costs tens of reads rather than thousands.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Queued stdin bytes above which `write_stdin` reports that the caller
/// should wait, which is what a writable stream's `write()` returning false
/// means.
const STDIN_HIGH_WATER_BYTES: usize = 1024 * 1024;

/// Everything one child can tell its owner, in the order it happened.
#[derive(Debug)]
pub enum ChildEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    StdoutEnd,
    StderrEnd,
    Exited(i32),
    /// Supervision of a live child failed partway.
    Failed(String),
    /// A write to the child's stdin failed while the child was still alive.
    StdinFailed(String),
    /// Everything queued for stdin has reached the child, after a write that
    /// reported the queue was too full to keep writing.
    StdinDrained,
}

/// What one platform's launcher hands back: a live process, its open pipes,
/// and the two operations only that platform knows how to perform.
pub(crate) struct PlatformChild {
    pub(crate) pid: u32,
    pub(crate) stdin: Option<Box<dyn Write + Send>>,
    pub(crate) stdout: Box<dyn Read + Send>,
    pub(crate) stderr: Box<dyn Read + Send>,
    /// Blocks until the child exits and reports its status.
    pub(crate) wait: Box<dyn FnOnce() -> io::Result<i32> + Send>,
    /// Ends the child and everything it started. `true` asks for the
    /// unconditional kill a platform offers over its polite signal.
    pub(crate) kill: Box<dyn Fn(bool) -> io::Result<()> + Send + Sync>,
}

/// What a caller has to describe to start one.
pub struct AsyncSpawn<'a> {
    pub executable: &'a str,
    pub arguments: &'a [String],
    pub cwd: Option<&'a Path>,
    pub environment: &'a [(OsString, OsString)],
    /// `environment` is the child's whole environment rather than a set of
    /// additions to this process's. Node's `spawn` means the first by `env`,
    /// and a caller handing a script a deliberately bare environment is
    /// relying on it.
    pub replace_environment: bool,
    /// The caller has already quoted the Windows command line itself.
    pub verbatim_arguments: bool,
    /// Give the child a pipe the owner writes to, rather than the null device.
    pub piped_stdin: bool,
}

struct Shared {
    state: Mutex<State>,
    room: Condvar,
}

struct State {
    events: VecDeque<ChildEvent>,
    buffered_bytes: usize,
    /// The owner is gone; every thread should stop where it is.
    closed: bool,
}

impl Shared {
    fn push(&self, event: ChildEvent) {
        let bytes = match &event {
            ChildEvent::Stdout(chunk) | ChildEvent::Stderr(chunk) => chunk.len(),
            _ => 0,
        };
        let mut state = lock(&self.state);
        if state.closed {
            return;
        }
        state.buffered_bytes += bytes;
        state.events.push_back(event);
        drop(state);
        signal_activity();
    }

    /// Waits until the queue has room, and reports whether the caller should
    /// keep reading at all.
    fn wait_for_room(&self) -> bool {
        let mut state = lock(&self.state);
        while !state.closed && state.buffered_bytes >= MAXIMUM_CHILD_BUFFERED_BYTES {
            let (next, _) = self
                .room
                .wait_timeout(state, Duration::from_millis(100))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
        }
        !state.closed
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One counter every child bumps, and one condition variable every owner can
/// wait on.
///
/// An event loop needs a single thing to block on: it has no idea which child
/// will speak next, and waking every millisecond to ask is what a runtime
/// does instead of waiting properly. The counter makes the wait race-free --
/// an owner reads the tick before it drains, so activity that lands between
/// the drain and the wait is already visible as a changed tick.
static ACTIVITY: LazyLock<(Mutex<u64>, Condvar)> =
    LazyLock::new(|| (Mutex::new(1), Condvar::new()));

fn signal_activity() {
    let (counter, signal) = &*ACTIVITY;
    let mut tick = lock(counter);
    *tick = tick.wrapping_add(1);
    drop(tick);
    signal.notify_all();
}

/// The current activity counter. Read it before draining, pass it to
/// [`wait_for_activity`] afterwards.
pub fn activity_tick() -> u64 {
    let (counter, _) = &*ACTIVITY;
    *lock(counter)
}

/// Blocks until the activity counter moves past `since`, or the timeout runs
/// out. Reports the counter it saw.
pub fn wait_for_activity(since: u64, timeout: Duration) -> u64 {
    let (counter, signal) = &*ACTIVITY;
    let tick = lock(counter);
    if *tick != since {
        return *tick;
    }
    let (tick, _) = signal
        .wait_timeout(tick, timeout)
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *tick
}

pub struct AsyncChild {
    pid: u32,
    shared: Arc<Shared>,
    stdin: Mutex<Option<StdinSink>>,
    kill: Box<dyn Fn(bool) -> io::Result<()> + Send + Sync>,
}

struct StdinSink {
    chunks: Sender<Vec<u8>>,
    queue: Arc<StdinQueue>,
}

/// What the writer thread and the owner both need to see about stdin: how
/// much is waiting, and whether anyone was told to stop writing.
struct StdinQueue {
    pending_bytes: AtomicUsize,
    /// A write reported backpressure and has not been told the queue emptied.
    awaiting_drain: AtomicBool,
}

impl AsyncChild {
    pub fn spawn(request: &AsyncSpawn<'_>) -> io::Result<Self> {
        if request.executable.is_empty() || request.executable.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native child input is invalid",
            ));
        }
        if request
            .arguments
            .iter()
            .any(|argument| argument.contains('\0'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child argument contains a null character",
            ));
        }
        let child = crate::launch_async(request)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                events: VecDeque::new(),
                buffered_bytes: 0,
                closed: false,
            }),
            room: Condvar::new(),
        });

        spawn_reader(Arc::clone(&shared), child.stdout, false);
        spawn_reader(Arc::clone(&shared), child.stderr, true);

        let stdin = child.stdin.map(|pipe| {
            let (chunks, chunk_queue) = channel::<Vec<u8>>();
            let queue = Arc::new(StdinQueue {
                pending_bytes: AtomicUsize::new(0),
                awaiting_drain: AtomicBool::new(false),
            });
            spawn_writer(Arc::clone(&shared), pipe, chunk_queue, Arc::clone(&queue));
            StdinSink { chunks, queue }
        });

        let waiter = child.wait;
        let exit_shared = Arc::clone(&shared);
        detach(move || {
            exit_shared.push(match waiter() {
                Ok(status) => ChildEvent::Exited(status),
                Err(cause) => ChildEvent::Failed(cause.to_string()),
            });
        });

        Ok(Self {
            pid: child.pid,
            shared,
            stdin: Mutex::new(stdin),
            kill: child.kill,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Moves everything the child has said so far to the caller, freeing the
    /// room its output was holding.
    pub fn drain(&self) -> Vec<ChildEvent> {
        let mut state = lock(&self.shared.state);
        if state.events.is_empty() {
            return Vec::new();
        }
        let events: Vec<ChildEvent> = state.events.drain(..).collect();
        state.buffered_bytes = 0;
        drop(state);
        self.shared.room.notify_all();
        events
    }

    /// Queues `bytes` for the child's stdin. Reports whether the queue is
    /// still below its high-water mark; a caller told otherwise should wait
    /// for [`ChildEvent::StdinDrained`] before writing more.
    pub fn write_stdin(&self, bytes: &[u8]) -> io::Result<bool> {
        let stdin = lock(&self.stdin);
        let Some(sink) = stdin.as_ref() else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "child stdin is closed",
            ));
        };
        let pending = sink
            .queue
            .pending_bytes
            .fetch_add(bytes.len(), Ordering::AcqRel)
            + bytes.len();
        if sink.chunks.send(bytes.to_vec()).is_err() {
            sink.queue
                .pending_bytes
                .fetch_sub(bytes.len(), Ordering::AcqRel);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "child stdin is closed",
            ));
        }
        if pending < STDIN_HIGH_WATER_BYTES {
            return Ok(true);
        }
        sink.queue.awaiting_drain.store(true, Ordering::Release);
        Ok(false)
    }

    /// Closes the child's stdin once everything already queued has been
    /// written. A child that reads to end of input sees it then, and not
    /// before.
    pub fn close_stdin(&self) {
        *lock(&self.stdin) = None;
    }

    pub fn kill(&self, force: bool) -> io::Result<()> {
        (self.kill)(force)
    }
}

impl Drop for AsyncChild {
    fn drop(&mut self) {
        // Releasing the handle ends the child: nothing is left that could
        // read its output or answer it, and a process still running against
        // a runtime that has forgotten it is a leak with a PID.
        let _ = (self.kill)(true);
        *lock(&self.stdin) = None;
        let mut state = lock(&self.shared.state);
        state.closed = true;
        state.events.clear();
        state.buffered_bytes = 0;
        drop(state);
        self.shared.room.notify_all();
    }
}

fn spawn_reader(shared: Arc<Shared>, mut stream: Box<dyn Read + Send>, is_stderr: bool) {
    detach(move || {
        let mut buffer = vec![0; READ_CHUNK_BYTES];
        loop {
            if !shared.wait_for_room() {
                return;
            }
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let chunk = buffer[..read].to_vec();
                    shared.push(if is_stderr {
                        ChildEvent::Stderr(chunk)
                    } else {
                        ChildEvent::Stdout(chunk)
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                // A closed pipe is how a child that exited reports end of
                // output on Windows; it is the same event as a clean EOF.
                Err(error) if is_disconnect(&error) => break,
                Err(error) => {
                    // The stream is over either way, so the end event still
                    // owes the owner its turn: a consumer waiting for both
                    // streams to close would otherwise wait forever.
                    shared.push(ChildEvent::Failed(error.to_string()));
                    break;
                }
            }
        }
        shared.push(if is_stderr {
            ChildEvent::StderrEnd
        } else {
            ChildEvent::StdoutEnd
        });
    });
}

fn spawn_writer(
    shared: Arc<Shared>,
    mut pipe: Box<dyn Write + Send>,
    chunks: Receiver<Vec<u8>>,
    queue: Arc<StdinQueue>,
) {
    detach(move || {
        loop {
            // Every sender being gone means the owner closed stdin, so the
            // child should see end of input. Dropping the pipe is what does
            // it.
            let Ok(chunk) = chunks.recv() else { return };
            let result = pipe.write_all(&chunk).and_then(|()| pipe.flush());
            let remaining =
                queue.pending_bytes.fetch_sub(chunk.len(), Ordering::AcqRel) - chunk.len();
            if let Err(error) = result {
                // A child that has already exited closes its end first, and
                // reporting that as a failure would turn an ordinary race
                // into an error the caller never made.
                if !is_disconnect(&error) {
                    shared.push(ChildEvent::StdinFailed(error.to_string()));
                }
                drain_quietly(&chunks, &queue);
                return;
            }
            if remaining == 0 && queue.awaiting_drain.swap(false, Ordering::AcqRel) {
                shared.push(ChildEvent::StdinDrained);
            }
        }
    });
}

/// Empties the queue without writing it, so a caller that keeps pushing after
/// the pipe broke does not grow the pending counter without bound.
fn drain_quietly(chunks: &Receiver<Vec<u8>>, queue: &StdinQueue) {
    loop {
        match chunks.try_recv() {
            Ok(chunk) => {
                queue.pending_bytes.fetch_sub(chunk.len(), Ordering::AcqRel);
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
        }
    }
}

fn is_disconnect(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof
    ) || (cfg!(windows) && error.raw_os_error() == Some(ERROR_BROKEN_PIPE))
}

const ERROR_BROKEN_PIPE: i32 = 109;

/// Starts a supervision thread. Nothing ever joins these: each one ends when
/// the pipe it owns closes, which is what the child exiting does to all of
/// them.
fn detach(body: impl FnOnce() + Send + 'static) {
    // Out of threads is a condition the caller cannot act on from here; the
    // child stays alive and its queue simply stays empty.
    let _ = std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// The same script for both shells this crate supports.
    fn shell(windows: &str, unix: &str) -> (String, Vec<String>) {
        if cfg!(windows) {
            (
                "cmd.exe".to_owned(),
                vec!["/d".to_owned(), "/c".to_owned(), windows.to_owned()],
            )
        } else {
            ("/bin/sh".to_owned(), vec!["-c".to_owned(), unix.to_owned()])
        }
    }

    fn start(windows: &str, unix: &str, piped_stdin: bool) -> AsyncChild {
        let (executable, arguments) = shell(windows, unix);
        AsyncChild::spawn(&AsyncSpawn {
            executable: &executable,
            arguments: &arguments,
            cwd: None,
            environment: &[],
            replace_environment: false,
            verbatim_arguments: false,
            piped_stdin,
        })
        .expect("child should start")
    }

    /// Drains until `stop` is satisfied or the deadline passes, blocking on
    /// the activity signal exactly as an event loop would.
    fn collect(child: &AsyncChild, stop: impl Fn(&[ChildEvent]) -> bool) -> Vec<ChildEvent> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            let tick = activity_tick();
            seen.extend(child.drain());
            if stop(&seen) {
                return seen;
            }
            wait_for_activity(tick, Duration::from_millis(50));
        }
        panic!("child did not reach the expected state in time");
    }

    fn text(events: &[ChildEvent]) -> String {
        let mut bytes = Vec::new();
        for event in events {
            if let ChildEvent::Stdout(chunk) = event {
                bytes.extend_from_slice(chunk);
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn exit_status(events: &[ChildEvent]) -> Option<i32> {
        events.iter().find_map(|event| match event {
            ChildEvent::Exited(status) => Some(*status),
            _ => None,
        })
    }

    #[test]
    fn reports_output_and_exit_status() {
        let child = start(
            "echo one& echo two& exit /b 3",
            "echo one; echo two; exit 3",
            false,
        );
        let events = collect(&child, |seen| exit_status(seen).is_some());
        assert_eq!(exit_status(&events), Some(3));
        let printed = text(&events);
        assert!(printed.contains("one"), "stdout: {printed}");
        assert!(printed.contains("two"), "stdout: {printed}");
    }

    /// The whole point of this type: a child that has not exited has already
    /// been heard from. Delivering its first line only at exit is the bug
    /// this replaces, so the assertion is that no exit has been seen yet.
    #[test]
    fn delivers_output_before_the_child_exits() {
        let child = start(
            "echo first& ping -n 4 127.0.0.1 >nul& echo second",
            "echo first; sleep 3; echo second",
            false,
        );
        let events = collect(&child, |seen| text(seen).contains("first"));
        assert!(
            exit_status(&events).is_none(),
            "first line arrived only at exit: {events:?}"
        );
    }

    /// A child that answers what it is told is what a build service is.
    #[test]
    fn writes_to_child_stdin() {
        let child = start("sort", "sort", true);
        child.write_stdin(b"beta\nalpha\n").expect("stdin write");
        child.close_stdin();
        let events = collect(&child, |seen| exit_status(seen).is_some());
        let printed = text(&events);
        let alpha = printed.find("alpha").expect("alpha should be printed");
        let beta = printed.find("beta").expect("beta should be printed");
        assert!(alpha < beta, "output was not sorted: {printed}");
    }

    #[test]
    fn ends_both_output_streams() {
        let child = start("echo done", "echo done", false);
        let events = collect(&child, |seen| {
            seen.iter()
                .any(|event| matches!(event, ChildEvent::StdoutEnd))
                && seen
                    .iter()
                    .any(|event| matches!(event, ChildEvent::StderrEnd))
        });
        assert!(text(&events).contains("done"));
    }

    #[test]
    fn kills_a_running_child() {
        let child = start("ping -n 120 127.0.0.1 >nul", "sleep 120", false);
        child.kill(true).expect("kill should reach the child");
        let events = collect(&child, |seen| exit_status(seen).is_some());
        assert!(exit_status(&events).is_some());
    }

    #[test]
    fn rejects_a_write_after_stdin_closes() {
        let child = start("sort", "sort", true);
        child.close_stdin();
        let error = child.write_stdin(b"late").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn refuses_an_empty_executable() {
        let started = AsyncChild::spawn(&AsyncSpawn {
            executable: "",
            arguments: &[],
            cwd: None,
            environment: &[],
            replace_environment: false,
            verbatim_arguments: false,
            piped_stdin: false,
        });
        let Err(error) = started else {
            panic!("an empty executable should not start");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
