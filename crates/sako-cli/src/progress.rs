// SPDX-License-Identifier: BSD-3-Clause

//! The live view `sako install`, `add`, `update`, and `remove` draw.
//!
//! An install is four stages that overlap, so a single line naming one package
//! was the wrong shape for it: it could only ever show one of the four, and it
//! spent most of a cold install stuck on whichever package the recursion
//! happened to be inside. A tree shows all four at once, with counts, which is
//! what makes a parallel installer look like one.
//!
//! ```text
//! install
//! ├─ resolve  38/72
//! │  ├─ @changesets/types
//! │  ├─ vite
//! │  └─ rolldown
//! ├─ fetch    12/19
//! ├─ store     7/12
//! └─ link      0/72
//! ```
//!
//! The reporting side of this is called from every worker in the pool, several
//! times per package, so none of it draws anything: it moves counters and
//! returns. A renderer thread paints from those counters on a timer, which is
//! the only way an install of ten thousand events does not spend its time in
//! the terminal. Rendering lives here rather than in `sako-package` so the
//! library stays free of terminal concerns: it reports, this decides what a
//! person sees.
//!
//! Everything goes to stderr, keeping stdout clean for anything that wants to
//! pipe an install's output.

use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sako_package::{ProgressEvent, ProgressReporter};

use crate::style::Painter;

/// How often the tree is repainted. Fast enough to look live, slow enough that
/// a graph producing tens of thousands of events repaints a few dozen times
/// rather than once per event.
const REDRAW_INTERVAL: Duration = Duration::from_millis(40);

/// Widest line drawn. Anything longer wraps, and a wrapped line breaks the
/// cursor arithmetic that erases the previous frame.
const LINE_WIDTH: usize = 78;

/// In-flight package names shown under the resolve row.
const SHOWN_NAMES: usize = 3;

/// Most warnings held for the renderer to print. Bounded because the workers
/// producing them must never block on the terminal, and an install that
/// generates more than this has bigger problems than a truncated log.
const MAXIMUM_PENDING: usize = 64;

/// One stage's progress.
#[derive(Default)]
struct Counter {
    total: AtomicUsize,
    done: AtomicUsize,
}

impl Counter {
    fn plan(&self, total: usize) {
        self.total.store(total, Ordering::Relaxed);
    }

    fn start(&self) {
        // Resolve has no plan: the graph is only known once it is walked, so
        // the denominator is what has been discovered so far.
        self.total.fetch_add(1, Ordering::Relaxed);
    }

    fn finish(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    fn read(&self) -> (usize, usize) {
        (
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }
}

#[derive(Default)]
struct Model {
    resolve: Counter,
    fetch: Counter,
    store: Counter,
    link: Counter,
    cached: AtomicUsize,
    fetched: AtomicUsize,
    planned: AtomicUsize,
    /// Names currently being resolved, for the rows under the resolve count.
    /// Bounded by the resolver's own concurrency limit.
    active: Mutex<Vec<String>>,
    pending: Mutex<VecDeque<String>>,
    /// Bumped by every event so the renderer can skip a frame that would be
    /// identical to the one already on screen.
    revision: AtomicUsize,
}

impl Model {
    fn touch(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    fn enter(&self, name: &str) {
        let mut active = self.active.lock().unwrap_or_else(|error| error.into_inner());
        if !active.iter().any(|held| held == name) {
            active.push(name.to_owned());
        }
    }

    fn leave(&self, name: &str) {
        let mut active = self.active.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(index) = active.iter().position(|held| held == name) {
            active.remove(index);
        }
    }

    fn warn(&self, message: String) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if pending.len() >= MAXIMUM_PENDING {
            pending.pop_front();
        }
        pending.push_back(message);
    }
}

pub struct Install {
    model: Arc<Model>,
    painter: Painter,
    /// Whether to paint a live tree at all. False when stderr is redirected,
    /// where cursor movement would fill a log with escape sequences.
    animate: bool,
    started: Instant,
    stop: Arc<AtomicBool>,
    renderer: Mutex<Option<JoinHandle<()>>>,
    /// Set once the summary has been printed, so a second `Finished` -- or a
    /// drop after one -- does not print it twice.
    done: AtomicBool,
}

impl Install {
    pub fn new() -> Self {
        let model = Arc::new(Model::default());
        let painter = Painter::stderr();
        let animate = animation_enabled();
        let stop = Arc::new(AtomicBool::new(false));
        let renderer = animate.then(|| {
            let model = Arc::clone(&model);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || render_loop(&model, painter, &stop))
        });
        if !animate {
            let mut output = std::io::stderr().lock();
            let _ = writeln!(output, "resolving dependencies...");
        }
        Self {
            model,
            painter,
            animate,
            started: Instant::now(),
            stop,
            renderer: Mutex::new(renderer),
            done: AtomicBool::new(false),
        }
    }

    /// Stops the renderer and leaves the terminal on a clean line.
    fn halt(&self) {
        self.stop.store(true, Ordering::Release);
        let handle = self
            .renderer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl Default for Install {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        // An install that failed never sends `Finished`, and the renderer
        // thread must not outlive the run that owns the terminal.
        self.halt();
        if !self.done.load(Ordering::Relaxed) {
            drain_warnings(&self.model, self.painter);
        }
    }
}

impl ProgressReporter for Install {
    fn report(&self, event: ProgressEvent<'_>) {
        let model = &self.model;
        match event {
            ProgressEvent::Planned { total } => model.planned.store(total, Ordering::Relaxed),
            ProgressEvent::ResolveStarted { name } => {
                model.resolve.start();
                model.enter(name);
            }
            ProgressEvent::ResolveFinished { name } => {
                model.resolve.finish();
                model.leave(name);
            }
            ProgressEvent::FetchPlanned { total } => model.fetch.plan(total),
            ProgressEvent::DownloadStarted { .. } => {}
            ProgressEvent::DownloadFinished { cached, .. } => {
                model.fetch.finish();
                if cached {
                    model.cached.fetch_add(1, Ordering::Relaxed);
                } else {
                    model.fetched.fetch_add(1, Ordering::Relaxed);
                }
            }
            ProgressEvent::ExtractStarted { .. } | ProgressEvent::ExtractFinished { .. } => {}
            ProgressEvent::StorePlanned { total } => model.store.plan(total),
            ProgressEvent::Materialized { .. } => model.store.finish(),
            ProgressEvent::LinkPlanned { total } => model.link.plan(total),
            ProgressEvent::Linked => model.link.finish(),
            ProgressEvent::Warning { message } => model.warn(message.to_owned()),
            ProgressEvent::Finished { installed } => {
                self.halt();
                self.done.store(true, Ordering::Relaxed);
                drain_warnings(model, self.painter);
                self.summarize(installed);
                return;
            }
        }
        model.touch();
    }
}

impl Install {
    fn summarize(&self, installed: usize) {
        let elapsed = self.started.elapsed();
        let cached = self.model.cached.load(Ordering::Relaxed);
        let fetched = self.model.fetched.load(Ordering::Relaxed);
        let (resolved, _) = self.model.resolve.read();
        let (linked, _) = self.model.link.read();
        let mut output = std::io::stderr().lock();

        if !self.animate {
            // Line-oriented, no cursor control: a redirected stream is a log,
            // and a log wants one fact per line.
            let _ = writeln!(output, "resolved {resolved} packages");
            let _ = writeln!(output, "fetched {fetched} packages");
            let _ = writeln!(output, "linked {linked} directories");
            let _ = writeln!(
                output,
                "done {installed} packages in {}",
                duration(elapsed.as_millis())
            );
            return;
        }

        let _ = writeln!(
            output,
            "{} {} in {}",
            self.painter.green("done"),
            self.painter.bold(&packages(installed)),
            self.painter.bold(&duration(elapsed.as_millis())),
        );
        let rows = [
            ("resolved", resolved),
            ("cached", cached),
            ("fetched", fetched),
            ("linked", linked),
        ];
        for (index, (label, value)) in rows.iter().enumerate() {
            let branch = if index + 1 == rows.len() {
                "└─"
            } else {
                "├─"
            };
            let _ = writeln!(
                output,
                "{} {:<9}{}",
                self.painter.dim(branch),
                self.painter.dim(label),
                self.painter.bold(&value.to_string()),
            );
        }
        let _ = output.flush();
    }
}

/// Prints whatever warnings the workers left, above the summary.
fn drain_warnings(model: &Model, painter: Painter) {
    let mut pending = model
        .pending
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if pending.is_empty() {
        return;
    }
    let mut output = std::io::stderr().lock();
    for message in pending.drain(..) {
        let _ = writeln!(output, "{} {message}", painter.yellow("warning"));
    }
    let _ = output.flush();
}

/// Repaints the tree on a timer until the install stops it.
///
/// The timer, rather than the events, is what drives this. Workers only ever
/// move counters, so however many events an install produces, the terminal
/// sees twenty-five frames a second and no more.
fn render_loop(model: &Model, painter: Painter, stop: &AtomicBool) {
    let mut painted = 0_usize;
    let mut last_revision = usize::MAX;
    while !stop.load(Ordering::Acquire) {
        // Warnings first: they scroll, and the tree is redrawn under them.
        {
            let mut pending = model
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !pending.is_empty() {
                let messages: Vec<String> = pending.drain(..).collect();
                drop(pending);
                let mut output = std::io::stderr().lock();
                erase(&mut output, painted);
                for message in messages {
                    let _ = writeln!(output, "{} {message}", painter.yellow("warning"));
                }
                let _ = output.flush();
                painted = 0;
                last_revision = usize::MAX;
            }
        }
        let revision = model.revision.load(Ordering::Relaxed);
        if revision != last_revision {
            last_revision = revision;
            let frame = frame(model, painter);
            let mut output = std::io::stderr().lock();
            erase(&mut output, painted);
            for line in &frame {
                let _ = writeln!(output, "{line}");
            }
            let _ = output.flush();
            painted = frame.len();
        }
        std::thread::sleep(REDRAW_INTERVAL);
    }
    let mut output = std::io::stderr().lock();
    erase(&mut output, painted);
    let _ = output.flush();
}

/// Moves the cursor back over the previous frame and blanks it.
fn erase(output: &mut impl Write, lines: usize) {
    if lines == 0 {
        return;
    }
    let _ = write!(output, "\x1b[{lines}A");
    for _ in 0..lines {
        let _ = writeln!(output, "\x1b[2K");
    }
    let _ = write!(output, "\x1b[{lines}A");
}

/// Builds one frame of the tree.
fn frame(model: &Model, painter: Painter) -> Vec<String> {
    let mut lines = vec![painter.bold("install")];
    let active: Vec<String> = model
        .active
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .take(SHOWN_NAMES)
        .cloned()
        .collect();

    let stages = [
        ("resolve", model.resolve.read(), true),
        ("fetch", model.fetch.read(), false),
        ("store", model.store.read(), false),
        ("link", model.link.read(), false),
    ];
    for (index, (label, (done, total), detailed)) in stages.iter().enumerate() {
        let last = index + 1 == stages.len() && (!detailed || active.is_empty());
        let branch = if last { "└─" } else { "├─" };
        lines.push(format!(
            "{} {:<9}{}",
            painter.dim(branch),
            label,
            painter.bold(&format!("{done}/{total}")),
        ));
        if !detailed {
            continue;
        }
        // The names under `resolve` are the answer to "what is it doing right
        // now", which a count alone never gives. They are also the proof that
        // several packages are moving at once.
        for (position, name) in active.iter().enumerate() {
            let inner = if position + 1 == active.len() {
                "└─"
            } else {
                "├─"
            };
            lines.push(format!(
                "{}  {} {}",
                painter.dim("│"),
                painter.dim(inner),
                painter.dim(&truncate(name, LINE_WIDTH.saturating_sub(8))),
            ));
        }
    }
    lines
}

/// Whether to paint a live tree. Attached terminals get one; redirected stderr
/// does not, because cursor movement leaves a log full of escape sequences.
/// `SAKO_PROGRESS` overrides both directions -- `0` for a terminal that should
/// stay quiet, anything else for a captured stream that is going to be
/// replayed somewhere that understands the escapes.
///
/// Read on every construction rather than cached, so a process that installs
/// twice with its output redirected differently the second time is not still
/// answering the first question.
fn animation_enabled() -> bool {
    match std::env::var("SAKO_PROGRESS") {
        Ok(value) => value != "0",
        Err(_) => std::io::stderr().is_terminal(),
    }
}

fn packages(count: usize) -> String {
    if count == 1 {
        "1 package".to_owned()
    } else {
        format!("{count} packages")
    }
}

/// Milliseconds under a second, seconds with one decimal above it. Anything
/// finer than that is noise next to a network round trip.
fn duration(milliseconds: u128) -> String {
    if milliseconds < 1_000 {
        format!("{milliseconds}ms")
    } else {
        format!("{:.1}s", milliseconds as f64 / 1_000.0)
    }
}

/// Trims from the left, keeping the tail. A scoped package name shares its
/// first characters with every sibling, so the end is the part that identifies
/// it.
fn truncate(text: &str, room: usize) -> String {
    let length = text.chars().count();
    if length <= room {
        return text.to_owned();
    }
    if room <= 3 {
        return String::new();
    }
    let tail: String = text.chars().skip(length - (room - 3)).collect();
    format!("...{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_short_labels_whole() {
        assert_eq!(truncate("react@19.2.8", 20), "react@19.2.8");
    }

    #[test]
    fn trims_long_labels_from_the_left() {
        let trimmed = truncate("@scope/a-very-long-package-name@1.0.0", 12);
        assert_eq!(trimmed.chars().count(), 12);
        assert!(trimmed.starts_with("..."));
        assert!(trimmed.ends_with("@1.0.0"));
    }

    #[test]
    fn formats_durations_either_side_of_a_second() {
        assert_eq!(duration(940), "940ms");
        assert_eq!(duration(3_450), "3.5s");
    }

    #[test]
    fn counts_packages_in_english() {
        assert_eq!(packages(1), "1 package");
        assert_eq!(packages(2), "2 packages");
    }

    #[test]
    fn the_tree_shows_every_stage_and_the_names_in_flight() {
        let model = Model::default();
        model.resolve.plan(72);
        model.resolve.done.store(38, Ordering::Relaxed);
        model.fetch.plan(19);
        model.enter("vite");
        model.enter("@changesets/types");
        let lines = frame(&model, Painter::for_stream(crate::style::Stream::Stdout));
        assert_eq!(lines[0], "install");
        assert!(lines[1].contains("resolve"));
        assert!(lines[1].contains("38/72"));
        assert!(lines.iter().any(|line| line.contains("@changesets/types")));
        assert!(lines.iter().any(|line| line.contains("fetch")));
        assert!(lines.iter().any(|line| line.contains("link")));
    }

    #[test]
    fn a_name_stops_being_shown_once_it_resolves() {
        let model = Model::default();
        model.enter("vite");
        model.leave("vite");
        let lines = frame(&model, Painter::for_stream(crate::style::Stream::Stdout));
        assert!(!lines.iter().any(|line| line.contains("vite")));
    }

    #[test]
    fn held_warnings_never_grow_without_bound() {
        let model = Model::default();
        for index in 0..(MAXIMUM_PENDING * 2) {
            model.warn(format!("warning {index}"));
        }
        let pending = model.pending.lock().unwrap();
        assert_eq!(pending.len(), MAXIMUM_PENDING);
        // The oldest were dropped, so the newest survived.
        assert!(pending.back().unwrap().ends_with(&format!("{}", MAXIMUM_PENDING * 2 - 1)));
    }
}
