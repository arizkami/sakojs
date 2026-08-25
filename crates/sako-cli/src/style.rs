// SPDX-License-Identifier: BSD-3-Clause

//! Terminal setup for the CLI: colour, and the console's output encoding.
//!
//! Deliberately dependency-free: the workspace keeps its dependency surface
//! small, and this needs only SGR escapes plus a couple of console calls on
//! Windows.
//!
//! Colour is decided per stream, because stdout is frequently redirected while
//! stderr stays attached to the terminal. Honours the `NO_COLOR` convention
//! (<https://no-color.org>) and `FORCE_COLOR`, which is what CI systems set
//! when they want colour through a pipe.

use std::env;
use std::io::IsTerminal;
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const BLUE: &str = "\x1b[34m";
pub const MAGENTA: &str = "\x1b[35m";
pub const CYAN: &str = "\x1b[36m";

/// Styling handle for one stream. Copy so it can be threaded through render
/// helpers without ceremony.
#[derive(Clone, Copy)]
pub struct Painter {
    enabled: bool,
}

impl Painter {
    pub fn for_stream(stream: Stream) -> Self {
        Self {
            enabled: colors_enabled(stream),
        }
    }

    pub fn stdout() -> Self {
        Self::for_stream(Stream::Stdout)
    }

    pub fn stderr() -> Self {
        Self::for_stream(Stream::Stderr)
    }

    /// Wraps `text` in `code`, or returns it untouched when colour is off, so
    /// callers never branch on whether styling is active.
    pub fn paint(self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("{code}{text}{RESET}")
        } else {
            text.to_owned()
        }
    }

    pub fn bold(self, text: &str) -> String {
        self.paint(BOLD, text)
    }

    pub fn dim(self, text: &str) -> String {
        self.paint(DIM, text)
    }

    pub fn red(self, text: &str) -> String {
        self.paint(RED, text)
    }

    pub fn green(self, text: &str) -> String {
        self.paint(GREEN, text)
    }

    pub fn yellow(self, text: &str) -> String {
        self.paint(YELLOW, text)
    }

    pub fn blue(self, text: &str) -> String {
        self.paint(BLUE, text)
    }

    pub fn magenta(self, text: &str) -> String {
        self.paint(MAGENTA, text)
    }

    pub fn cyan(self, text: &str) -> String {
        self.paint(CYAN, text)
    }

    pub fn heading(self, text: &str) -> String {
        self.paint(&format!("{BOLD}{YELLOW}"), text)
    }

    pub fn command(self, text: &str) -> String {
        self.paint(&format!("{BOLD}{CYAN}"), text)
    }
}

/// Switches the console to UTF-8 output, returning the code page it replaced.
///
/// Sako writes UTF-8, and so does every tool it launches, but a Windows
/// console decodes output bytes with its own code page -- still an OEM one on
/// most machines. Vite's `->` arrow then arrives as three characters of
/// Cyrillic, and so does every emoji a tool prints.
///
/// This is `chcp 65001` performed by the process that needs it. The code page
/// belongs to the console rather than to the program, so a child process
/// inheriting these handles gets it too, which is the point: the mangled
/// output usually comes from the dev server, not from Sako.
///
/// Only the output side is touched. Switching the input code page as well is
/// the traditional advice and the traditional source of broken console reads,
/// and nothing here needs it.
#[cfg(windows)]
pub fn use_utf8_output() -> Option<u32> {
    const CP_UTF8: u32 = 65001;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleOutputCP() -> u32;
        fn SetConsoleOutputCP(code_page: u32) -> i32;
    }

    unsafe {
        let previous = GetConsoleOutputCP();
        // Zero means there is no console attached, and a console already in
        // UTF-8 needs nothing restored.
        if previous == 0 || previous == CP_UTF8 {
            return None;
        }
        (SetConsoleOutputCP(CP_UTF8) != 0).then_some(previous)
    }
}

/// Puts back whatever `use_utf8_output` replaced, so a shell session is left
/// as it was found.
#[cfg(windows)]
pub fn restore_output_encoding(previous: Option<u32>) {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleOutputCP(code_page: u32) -> i32;
    }

    if let Some(code_page) = previous {
        unsafe {
            SetConsoleOutputCP(code_page);
        }
    }
}

/// POSIX consoles are UTF-8 already; there is no per-console encoding to set.
#[cfg(not(windows))]
pub fn use_utf8_output() -> Option<u32> {
    None
}

#[cfg(not(windows))]
pub fn restore_output_encoding(_previous: Option<u32>) {}

fn colors_enabled(stream: Stream) -> bool {
    static STDOUT: OnceLock<bool> = OnceLock::new();
    static STDERR: OnceLock<bool> = OnceLock::new();
    let cell = match stream {
        Stream::Stdout => &STDOUT,
        Stream::Stderr => &STDERR,
    };
    *cell.get_or_init(|| detect(stream))
}

fn detect(stream: Stream) -> bool {
    // Presence alone disables, whatever the value -- that is the NO_COLOR rule.
    if env::var_os("NO_COLOR").is_some() {
        return false;
    }
    let forced = matches!(env::var("FORCE_COLOR"), Ok(value) if value != "0");
    if !forced {
        if matches!(env::var("TERM"), Ok(term) if term == "dumb") {
            return false;
        }
        let attached = match stream {
            Stream::Stdout => std::io::stdout().is_terminal(),
            Stream::Stderr => std::io::stderr().is_terminal(),
        };
        if !attached {
            return false;
        }
    }
    enable_virtual_terminal(stream)
}

/// Windows consoles need ENABLE_VIRTUAL_TERMINAL_PROCESSING before they
/// interpret SGR escapes. Windows Terminal and PowerShell 7 set it already;
/// plain `cmd.exe` does not, and would otherwise print raw escape bytes.
#[cfg(windows)]
fn enable_virtual_terminal(stream: Stream) -> bool {
    use std::ffi::c_void;

    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> *mut c_void;
        fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
    }

    let which = match stream {
        Stream::Stdout => STD_OUTPUT_HANDLE,
        Stream::Stderr => STD_ERROR_HANDLE,
    };
    unsafe {
        let handle = GetStdHandle(which);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut mode = 0u32;
        if GetConsoleMode(handle, &mut mode) == 0 {
            // Not a console (redirected). FORCE_COLOR can still want escapes.
            return env::var_os("FORCE_COLOR").is_some();
        }
        if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0 {
            return true;
        }
        SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

#[cfg(not(windows))]
fn enable_virtual_terminal(_stream: Stream) -> bool {
    true
}
