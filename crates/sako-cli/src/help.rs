// SPDX-License-Identifier: BSD-3-Clause

//! Help output.
//!
//! The command table below is the single source for both the overview and the
//! per-command pages, so `sako --help` and `sako help <command>` cannot drift
//! apart as commands are added.

use crate::style::{BOLD, MAGENTA, Painter};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Command {
    pub name: &'static str,
    pub usage: &'static str,
    pub summary: &'static str,
    pub group: Group,
    pub details: &'static [&'static str],
    pub examples: &'static [(&'static str, &'static str)],
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Group {
    Run,
    Packages,
}

impl Group {
    fn title(self) -> &'static str {
        match self {
            Group::Run => "RUNNING CODE",
            Group::Packages => "PACKAGES",
        }
    }
}

pub const COMMANDS: &[Command] = &[
    Command {
        name: "run",
        usage: "sako run <script|name> [args...]",
        summary: "Run a file, or a script from package.json",
        group: Group::Run,
        details: &[
            "Given a path, runs that file. Otherwise looks up the name in the",
            "\"scripts\" object of package.json and runs it through the system shell.",
            "",
            "Every node_modules/.bin directory from the current directory upwards is",
            "prepended to PATH, so scripts can call locally installed tools by bare",
            "name. Output streams straight through, and the script's exit code",
            "becomes Sako's exit code.",
        ],
        examples: &[
            ("sako run dev", "run the \"dev\" script"),
            ("sako run build --minify", "pass arguments through"),
            ("sako run ./server.ts", "run a file directly"),
        ],
    },
    Command {
        name: "x",
        usage: "sako x <command> [args...]",
        summary: "Execute a package binary, like npx or bunx",
        group: Group::Run,
        details: &[
            "Resolves <command> against node_modules/.bin, walking up from the",
            "current directory, and executes it with the arguments that follow.",
            "",
            "Arguments are never interpreted by Sako: everything after <command> is",
            "handed to the tool untouched, so flags like --help reach the tool",
            "instead of being claimed by Sako.",
        ],
        examples: &[
            (
                "sako x tsc --noEmit",
                "type-check with the local TypeScript",
            ),
            ("sako x vite build", "run the local Vite"),
        ],
    },
    Command {
        name: "create",
        usage: "sako create <initializer> [args...]",
        summary: "Scaffold a project from an initializer package",
        group: Group::Run,
        details: &[
            "Follows the convention npm, bun, and yarn share: `create vite` runs the",
            "package `create-vite`. A scope works too -- `create @acme/app` runs",
            "@acme/create-app, and `create @acme` runs @acme/create.",
            "",
            "The initializer is fetched into a cache and run from there, so nothing",
            "is written into the current directory except what the initializer",
            "itself creates. A locally installed initializer takes precedence, so a",
            "repository can pin its own.",
            "",
            "Pin a version by appending it to the name; each version is cached",
            "separately.",
        ],
        examples: &[
            ("sako create vite my-app", "scaffold with create-vite"),
            ("sako create vite@7 my-app", "pin the initializer version"),
            ("sako create @acme/app", "run @acme/create-app"),
        ],
    },
    Command {
        name: "eval",
        usage: "sako eval <source> [args...]",
        summary: "Evaluate JavaScript from the command line",
        group: Group::Run,
        details: &["Also available as -e."],
        examples: &[("sako eval \"console.log(1 + 1)\"", "")],
    },
    Command {
        name: "repl",
        usage: "sako repl",
        summary: "Start an interactive session",
        group: Group::Run,
        details: &["Exit with .exit, .quit, or end-of-file."],
        examples: &[],
    },
    Command {
        name: "install",
        usage: "sako install",
        summary: "Install everything in package.json",
        group: Group::Packages,
        details: &[
            "Also links each dependency's declared binaries into node_modules/.bin",
            "so package scripts and `sako x` can find them.",
            "",
            "Progress is drawn on stderr while packages are resolved and unpacked,",
            "and only when stderr is a terminal. SAKO_PROGRESS=0 turns it off,",
            "SAKO_PROGRESS=1 forces it on for a captured stream.",
        ],
        examples: &[],
    },
    Command {
        name: "add",
        usage: "sako add <package>... [--dev]",
        summary: "Add dependencies and install them",
        group: Group::Packages,
        details: &["-D is accepted as a synonym for --dev."],
        examples: &[
            ("sako add lodash", ""),
            ("sako add --dev vitest", "add a dev dependency"),
        ],
    },
    Command {
        name: "remove",
        usage: "sako remove <package>...",
        summary: "Remove dependencies",
        group: Group::Packages,
        details: &[],
        examples: &[],
    },
    Command {
        name: "update",
        usage: "sako update",
        summary: "Update dependencies within their declared ranges",
        group: Group::Packages,
        details: &[],
        examples: &[],
    },
];

pub const OPTIONS: &[(&str, &str)] = &[
    ("-h, --help", "Show help; add a command name for detail"),
    ("-V, --version", "Print the version"),
    ("--workers=N|auto", "Run a script on N runtimes in parallel"),
    ("--memory-stats", "Print runtime memory usage on exit"),
    ("--detect-leaks", "Fail if resources are still held at exit"),
    ("--perf-breakdown", "Print startup and execution timings"),
];

pub const PACKAGE_OPTIONS: &[(&str, &str)] = &[
    ("--registry=<url>", "Registry to fetch from"),
    ("--token=<token>", "Registry auth token"),
    ("--proxy=<url>", "HTTP proxy for registry requests"),
    ("--ignore-scripts", "Skip lifecycle scripts"),
];

pub fn find(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.name == name)
}

/// Pads to a *visible* width.
///
/// `{:<width$}` counts bytes, and SGR escapes are bytes that occupy no
/// columns, so formatting a styled string directly under-pads by the escape
/// length -- a different amount for every style. Measuring the unstyled text
/// keeps colour and no-colour output aligned identically.
fn column(painted: &str, visible: &str, width: usize) -> String {
    let mut padded = painted.to_owned();
    padded.push_str(&" ".repeat(width.saturating_sub(visible.chars().count())));
    padded
}

fn rows(
    painter: Painter,
    entries: &[(&str, &str)],
    width: usize,
    accent: fn(Painter, &str) -> String,
) -> String {
    let mut out = String::new();
    for (left, right) in entries {
        out.push_str(&format!(
            "    {}  {}\n",
            column(&accent(painter, left), left, width),
            right,
        ));
    }
    out
}

pub fn overview(painter: Painter) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\n  {} {}\n  {}\n\n",
        painter.paint(&format!("{BOLD}{MAGENTA}"), "sako"),
        painter.dim(VERSION),
        painter.dim("A JavaScript and TypeScript runtime"),
    ));

    out.push_str(&format!("  {}\n", painter.heading("USAGE")));
    out.push_str(&format!(
        "    {} {}\n\n",
        painter.command("sako"),
        painter.dim("<command> [options] [args...]"),
    ));

    for group in [Group::Run, Group::Packages] {
        out.push_str(&format!("  {}\n", painter.heading(group.title())));
        for command in COMMANDS.iter().filter(|entry| entry.group == group) {
            out.push_str(&format!(
                "    {}  {}\n",
                column(&painter.command(command.name), command.name, 10),
                command.summary,
            ));
        }
        out.push('\n');
    }

    out.push_str(&format!("  {}\n", painter.heading("OPTIONS")));
    out.push_str(&rows(painter, OPTIONS, 20, |painter, text| {
        painter.green(text)
    }));
    out.push('\n');

    out.push_str(&format!("  {}\n", painter.heading("EXAMPLES")));
    for (example, note) in [
        ("sako server.ts", "run a file"),
        ("sako run dev", "run a package script"),
        ("sako x tsc --noEmit", "run a local tool"),
        ("sako add hono", "add a dependency"),
    ] {
        out.push_str(&format!(
            "    {}  {}\n",
            column(&painter.cyan(example), example, 24),
            painter.dim(note),
        ));
    }

    out.push_str(&format!(
        "\n  {}\n\n",
        painter.dim("Run `sako help <command>` for more about a command."),
    ));
    out
}

pub fn command_page(painter: Painter, command: &Command) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\n  {}  {}\n\n",
        painter.command(command.name),
        painter.dim(command.summary),
    ));
    out.push_str(&format!("  {}\n", painter.heading("USAGE")));
    out.push_str(&format!("    {}\n\n", painter.cyan(command.usage)));

    if !command.details.is_empty() {
        for line in command.details {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str(&format!("  {line}\n"));
            }
        }
        out.push('\n');
    }

    if command.group == Group::Packages {
        out.push_str(&format!("  {}\n", painter.heading("OPTIONS")));
        out.push_str(&rows(painter, PACKAGE_OPTIONS, 20, |painter, text| {
            painter.green(text)
        }));
        out.push('\n');
    }

    if !command.examples.is_empty() {
        out.push_str(&format!("  {}\n", painter.heading("EXAMPLES")));
        for (example, note) in command.examples {
            out.push_str(&format!(
                "    {}  {}\n",
                column(&painter.cyan(example), example, 30),
                painter.dim(note),
            ));
        }
        out.push('\n');
    }
    out
}
