// SPDX-License-Identifier: BSD-3-Clause

//! The progress bar `sako install`, `add`, `update`, and `remove` draw.
//!
//! Installs used to print nothing between the command and its prompt. On a
//! warm store that is a fraction of a second; on a cold one it is minutes of
//! downloading with no sign the process is alive, which reads as a hang.
//!
//! Rendering lives here rather than in `sako-package` so the library stays
//! free of terminal concerns: it reports events, this decides what a person
//! sees. Everything goes to stderr, keeping stdout clean for anything that
//! wants to pipe an install's output.

use std::cell::RefCell;
use std::io::{IsTerminal, Write};
use std::time::Instant;

use sako_package::{ProgressEvent, ProgressReporter};

use crate::style::Painter;

/// Width of the bar itself, excluding the brackets. Fixed rather than derived
/// from the terminal, because the package name to its right is what varies and
/// the bar is easier to read when it does not resize under a long scoped name.
const BAR_WIDTH: usize = 24;

/// Widest line the bar will draw. Anything longer wraps, and a wrapped line
/// cannot be erased by a carriage return -- the tail stays on screen and every
/// redraw leaves another copy behind.
const LINE_WIDTH: usize = 78;

/// Frames for the case where no total is known yet. Plain ASCII: the rest of
/// the CLI's output is, and a code page that renders box drawing as mojibake
/// is still common on Windows.
const SPINNER: [char; 4] = ['|', '/', '-', '\\'];

pub struct Bar {
    painter: Painter,
    /// Whether to draw a live line at all. False when stderr is redirected,
    /// where carriage returns would fill a log with half-erased duplicates;
    /// the summary and any warnings still print.
    animate: bool,
    state: RefCell<State>,
}

struct State {
    started: Instant,
    /// Known only when the install came from a lockfile. Without it there is
    /// no denominator, so the bar becomes a spinner and a count.
    total: Option<usize>,
    installed: usize,
    downloaded: usize,
    frame: usize,
    /// Characters the last redraw left on the line, so the next one knows how
    /// much to blank. Cheaper and more portable than an erase-line escape,
    /// which a terminal without VT processing would print literally.
    painted: usize,
}

impl Bar {
    pub fn new() -> Self {
        Self {
            painter: Painter::stderr(),
            animate: animation_enabled(),
            state: RefCell::new(State {
                started: Instant::now(),
                total: None,
                installed: 0,
                downloaded: 0,
                frame: 0,
                painted: 0,
            }),
        }
    }

    /// Draws `line`, padded to cover whatever the previous one left behind,
    /// and parks the cursor back at column zero for the next redraw.
    ///
    /// `visible` is the number of columns the line occupies, which is not its
    /// length: the colour escapes inside it print nothing. Padding by the
    /// string length instead would emit a run of spaces wider than the
    /// terminal, wrapping the very line the next redraw means to overwrite.
    fn paint(&self, state: &mut State, line: &str, visible: usize) {
        if !self.animate {
            return;
        }
        let mut output = std::io::stderr().lock();
        let _ = write!(output, "\r{line}");
        for _ in visible..state.painted {
            let _ = write!(output, " ");
        }
        let _ = write!(output, "\r");
        let _ = output.flush();
        state.painted = visible;
    }

    /// Blanks the live line so an ordinary message can be printed under it.
    fn clear(&self, state: &mut State) {
        if !self.animate || state.painted == 0 {
            return;
        }
        let mut output = std::io::stderr().lock();
        let _ = write!(output, "\r");
        for _ in 0..state.painted {
            let _ = write!(output, " ");
        }
        let _ = write!(output, "\r");
        let _ = output.flush();
        state.painted = 0;
    }

    fn render(&self, state: &mut State, label: &str) {
        let head = match state.total {
            Some(total) => {
                let filled = if total == 0 {
                    BAR_WIDTH
                } else {
                    (state.installed.min(total) * BAR_WIDTH) / total
                };
                format!(
                    "{}{}{}{} {}",
                    self.painter.dim("["),
                    self.painter.cyan(&"#".repeat(filled)),
                    self.painter.dim(&"-".repeat(BAR_WIDTH - filled)),
                    self.painter.dim("]"),
                    self.painter
                        .bold(&format!("{}/{total}", state.installed.min(total))),
                )
            }
            None => {
                let frame = SPINNER[state.frame % SPINNER.len()];
                state.frame += 1;
                format!(
                    "{} {}",
                    self.painter.cyan(&frame.to_string()),
                    self.painter.bold(&state.installed.to_string()),
                )
            }
        };
        // Budget for the label is whatever the head did not use. Measured on
        // the unpainted text: the escapes the painter adds occupy no columns.
        let used = match state.total {
            Some(total) => {
                BAR_WIDTH + 3 + format!("{}/{total}", state.installed.min(total)).chars().count()
            }
            None => 2 + state.installed.to_string().chars().count(),
        };
        let room = LINE_WIDTH.saturating_sub(used + 4);
        let label = truncate(label, room);
        let visible = 4 + used + label.chars().count();
        let line = format!("  {head}  {}", self.painter.dim(&label));
        self.paint(state, &line, visible);
    }
}

impl ProgressReporter for Bar {
    fn report(&self, event: ProgressEvent<'_>) {
        let mut state = self.state.borrow_mut();
        match event {
            ProgressEvent::Planned { total } => {
                state.total = Some(total);
                self.render(&mut state, "");
            }
            ProgressEvent::Resolving { name } => {
                let label = format!("resolving {name}");
                self.render(&mut state, &label);
            }
            ProgressEvent::Installed {
                name,
                version,
                downloaded,
            } => {
                state.installed += 1;
                if downloaded {
                    state.downloaded += 1;
                }
                let label = format!("{name}@{version}");
                self.render(&mut state, &label);
            }
            ProgressEvent::Warning { message } => {
                self.clear(&mut state);
                eprintln!("{} {message}", self.painter.yellow("warning"));
                if state.installed != 0 {
                    self.render(&mut state, "");
                }
            }
            ProgressEvent::Finished { installed } => {
                self.clear(&mut state);
                let elapsed = state.started.elapsed();
                let cached = installed.saturating_sub(state.downloaded);
                eprintln!(
                    "{} {} in {}{}",
                    self.painter.green("done"),
                    self.painter.bold(&packages(installed)),
                    self.painter.bold(&duration(elapsed.as_millis())),
                    self.painter
                        .dim(&format!(" ({cached} from the store, {} fetched)", state.downloaded)),
                );
            }
        }
    }
}

/// Whether to draw a live line. Attached terminals get one; redirected stderr
/// does not, because a carriage return leaves a log full of half-erased
/// duplicates. `SAKO_PROGRESS` overrides both directions -- `0` for a terminal
/// that should stay quiet, anything else for a captured stream that is going
/// to be replayed somewhere that understands the escapes.
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
}
