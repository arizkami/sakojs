// SPDX-License-Identifier: BSD-3-Clause

//! A bounded pool of threads draining a queue that grows while they drain it.
//!
//! The installer's stages are all the same shape: a set of independent work
//! items, a limit on how many may be in flight, and -- for the resolver -- the
//! ability for finishing one item to reveal several more. That last part is
//! why this is a queue with an idle count rather than a `map` over a slice: the
//! graph is not known until it has been walked, so the pool has to be able to
//! tell "no work right now because everyone is busy producing more" apart from
//! "no work ever again".
//!
//! There is no async runtime in this workspace and this does not introduce
//! one. The HTTP client is blocking, so a thread parked in `read` is the unit
//! of concurrency, and the bound on how many may be parked at once is simply
//! how many threads the pool was given.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

/// A shared stop flag.
///
/// Set when a required piece of work fails, so the other workers stop taking
/// new items instead of spending another thirty network round trips producing
/// errors nobody will read. Work already in flight is allowed to finish on its
/// own -- a download interrupted halfway leaves a partial file, and unwinding
/// through the normal path is what removes it.
#[derive(Debug, Default)]
pub struct Cancel {
    stopped: AtomicBool,
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    #[inline]
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

struct State<T> {
    items: VecDeque<T>,
    /// Workers holding an item they have not finished yet. The queue being
    /// empty only means the run is over when this is zero too.
    busy: usize,
    /// Set once every worker should leave, whether the work ran out or a
    /// failure made the rest of it pointless.
    draining: bool,
}

/// The queue workers pull from, and push back into.
pub struct Queue<T> {
    state: Mutex<State<T>>,
    wake: Condvar,
}

impl<T> Queue<T> {
    fn new(items: impl IntoIterator<Item = T>) -> Self {
        Self {
            state: Mutex::new(State {
                items: items.into_iter().collect(),
                busy: 0,
                draining: false,
            }),
            wake: Condvar::new(),
        }
    }

    /// Adds work discovered while doing other work.
    pub fn push(&self, item: T) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.draining {
            return;
        }
        state.items.push_back(item);
        self.wake.notify_one();
    }

    /// Takes the next item, blocking while other workers might still produce
    /// one, and returning `None` once nothing more can arrive.
    fn pop(&self) -> Option<T> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if state.draining {
                return None;
            }
            if let Some(item) = state.items.pop_front() {
                state.busy += 1;
                return Some(item);
            }
            if state.busy == 0 {
                // Nothing queued and nobody working: the graph is closed.
                state.draining = true;
                self.wake.notify_all();
                return None;
            }
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    /// Marks the item a worker took as done, waking anyone parked on the
    /// possibility that this worker was going to produce more.
    fn finish(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.busy -= 1;
        if state.busy == 0 && state.items.is_empty() {
            state.draining = true;
        }
        self.wake.notify_all();
    }

    /// Abandons whatever is queued and releases every parked worker.
    fn drain(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.draining = true;
        state.items.clear();
        self.wake.notify_all();
    }
}

/// Runs `body` over `items` on at most `workers` threads, stopping early if a
/// call fails.
///
/// Returns the first error in the order the *queue* produced it rather than
/// the order the failures happened, which keeps the message a user sees from
/// depending on which thread lost a race. Later failures are dropped: they are
/// almost always the same registry being unreachable a second time, and
/// printing thirty of them buries the one that matters.
pub fn run<T, E, F>(
    workers: usize,
    items: impl IntoIterator<Item = T>,
    cancel: &Cancel,
    body: F,
) -> Result<(), E>
where
    T: Send,
    E: Send,
    F: Fn(T, &Queue<T>) -> Result<(), E> + Sync,
{
    let queue = Queue::new(items);
    // Sequence number rather than a plain slot, so "first" means first taken
    // off the queue and not first to finish failing.
    let failure: Mutex<Option<(usize, E)>> = Mutex::new(None);
    let workers = workers.max(1);
    // Assigned at pop time so the ordering is the queue's, shared across every
    // worker, rather than each worker's private count of its own items.
    let sequence = std::sync::atomic::AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while let Some(item) = queue.pop() {
                    let taken = sequence.fetch_add(1, Ordering::Relaxed);
                    if cancel.stopped() {
                        queue.finish();
                        queue.drain();
                        break;
                    }
                    let outcome = body(item, &queue);
                    queue.finish();
                    if let Err(error) = outcome {
                        let mut failure =
                            failure.lock().unwrap_or_else(|error| error.into_inner());
                        if failure.as_ref().is_none_or(|(first, _)| taken < *first) {
                            *failure = Some((taken, error));
                        }
                        drop(failure);
                        cancel.stop();
                        queue.drain();
                        break;
                    }
                }
            });
        }
    });

    match failure.into_inner().unwrap_or_else(|error| error.into_inner()) {
        Some((_, error)) => Err(error),
        None => Ok(()),
    }
}

/// How many threads to give a stage.
///
/// The three kinds of stage want different numbers, and giving them all the
/// same one leaves throughput on the table at both ends:
///
/// - **Network** stages want more threads than the machine has cores. A thread
///   blocked in `read` on a registry is not using a core, so the limit is
///   about how many connections a registry should be asked to serve at once,
///   not about the machine.
/// - **Decompression** wants about half the cores. It is the one genuinely
///   CPU-bound stage, and every worker holds a whole decompressed package in
///   memory while it runs, so more of them buys contention and memory
///   pressure rather than speed.
/// - **Filesystem** stages -- copying packages into place, writing `.bin`
///   shims -- want roughly one per core. They are neither: mostly waiting on
///   the disk, but with enough per-file work that the measured curve keeps
///   improving to about the core count and turns back down after it.
///
/// Every one of them is overridable, because these are measurements from one
/// machine and the right numbers on a laptop with a slow disk are not the
/// right numbers in CI.
pub fn limits() -> Limits {
    let cores = std::thread::available_parallelism()
        .map(|cores| cores.get())
        .unwrap_or(4);
    Limits {
        metadata: env_limit("SAKO_METADATA_CONCURRENCY", 16),
        download: env_limit("SAKO_DOWNLOAD_CONCURRENCY", 12),
        extract: env_limit("SAKO_EXTRACT_CONCURRENCY", (cores / 2).clamp(2, 8)),
        materialize: env_limit("SAKO_MATERIALIZE_CONCURRENCY", cores.clamp(2, 16)),
        link: env_limit("SAKO_LINK_CONCURRENCY", cores.clamp(2, 16)),
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub metadata: usize,
    pub download: usize,
    pub extract: usize,
    pub materialize: usize,
    pub link: usize,
}

/// Reads an override, ignoring one that is not a usable thread count rather
/// than failing an install over a typo in an environment variable.
fn env_limit(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0 && *value <= 256)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn runs_every_item() {
        let seen = AtomicUsize::new(0);
        let cancel = Cancel::new();
        let outcome: Result<(), ()> = run(4, 0..100, &cancel, |_, _| {
            seen.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        assert!(outcome.is_ok());
        assert_eq!(seen.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn work_may_produce_more_work() {
        let seen = AtomicUsize::new(0);
        let cancel = Cancel::new();
        // A tree: each item under 8 pushes two children, so 15 run in total.
        let outcome: Result<(), ()> = run(4, [1_usize], &cancel, |item, queue| {
            seen.fetch_add(1, Ordering::Relaxed);
            if item < 8 {
                queue.push(item * 2);
                queue.push(item * 2 + 1);
            }
            Ok(())
        });
        assert!(outcome.is_ok());
        assert_eq!(seen.load(Ordering::Relaxed), 15);
    }

    #[test]
    fn a_failure_stops_the_rest_and_is_reported() {
        let cancel = Cancel::new();
        let outcome: Result<(), &str> = run(4, 0..1_000, &cancel, |item, _| {
            if item == 0 { Err("first") } else { Ok(()) }
        });
        assert_eq!(outcome, Err("first"));
        assert!(cancel.stopped());
    }

    #[test]
    fn a_cancelled_run_stops_taking_work() {
        let seen = AtomicUsize::new(0);
        let cancel = Cancel::new();
        cancel.stop();
        let outcome: Result<(), ()> = run(4, 0..1_000, &cancel, |_, _| {
            seen.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        assert!(outcome.is_ok());
        assert_eq!(seen.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn an_empty_queue_finishes_immediately() {
        let cancel = Cancel::new();
        let outcome: Result<(), ()> = run(8, Vec::<usize>::new(), &cancel, |_, _| Ok(()));
        assert!(outcome.is_ok());
    }
}
