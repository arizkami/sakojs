// SPDX-License-Identifier: BSD-3-Clause

mod create;
mod help;
mod node_bin;
mod progress;
mod style;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sako_package::{PackageManager, PackageManagerOptions};
use sako_v8::{MemoryStats, Runtime, perf_enable, perf_mark, perf_report};

use style::Painter;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy)]
struct DiagnosticsOptions {
    print_memory: bool,
    detect_leaks: bool,
}

fn main() -> ! {
    let status = match run() {
        Ok(code) => code,
        Err(error) => {
            let painter = Painter::stderr();
            eprintln!("{} {error}", painter.red("error"));
            FAILURE
        }
    };
    perf_report();
    // Deliberately not returning: see sako_process::exit_immediately. Rust's
    // own streams are line buffered, so anything printed without a trailing
    // newline is still sitting in them and has to be flushed by hand.
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
    sako_process::exit_immediately(status)
}

/// Process exit codes, as plain numbers rather than `ExitCode`, which cannot
/// be turned back into one for `process::exit`.
const SUCCESS: u8 = 0;
const FAILURE: u8 = 1;

fn run() -> Result<u8, String> {
    let mut arguments: Vec<OsString> = env::args_os().skip(1).collect();

    // `x` and `create` forward everything after the command name, so the tool
    // receives its own --help and --version rather than Sako intercepting
    // them. Split here, before any global flag parsing touches the tail.
    if let Some(position) = arguments
        .iter()
        .position(|argument| argument == "x" || argument == "create")
    {
        if arguments[..position]
            .iter()
            .all(|argument| is_global_flag(argument))
        {
            let forwarding = arguments[position].clone();
            let forwarded = arguments.split_off(position + 1);
            return if forwarding == "create" {
                create::run(&forwarded)
            } else {
                execute_package_binary(&forwarded)
            };
        }
    }

    if take_flag(&mut arguments, "--perf-breakdown") {
        perf_enable();
    }
    let diagnostics = DiagnosticsOptions {
        print_memory: take_flag(&mut arguments, "--memory-stats"),
        detect_leaks: take_flag(&mut arguments, "--detect-leaks"),
    };
    let workers = take_workers(&mut arguments)?;
    let Some(command) = arguments.first().cloned() else {
        print!("{}", help::overview(Painter::stdout()));
        return Ok(SUCCESS);
    };
    arguments.remove(0);

    match command.to_string_lossy().as_ref() {
        "--help" | "-h" | "help" => {
            let painter = Painter::stdout();
            match arguments.first() {
                Some(topic) => {
                    let topic = topic.to_string_lossy().into_owned();
                    match help::find(&topic) {
                        Some(entry) => print!("{}", help::command_page(painter, entry)),
                        None => return Err(unknown_command(&topic)),
                    }
                }
                None => print!("{}", help::overview(painter)),
            }
            Ok(SUCCESS)
        }
        "--version" | "-V" => {
            println!("sako {VERSION}");
            Ok(SUCCESS)
        }
        "eval" | "-e" => {
            require_single_worker(workers, "eval")?;
            let Some(source) = arguments.first() else {
                return Err("eval requires JavaScript source".into());
            };
            let source = source.to_string_lossy().into_owned();
            let script_arguments = arguments[1..]
                .iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            execute_source(&source, "[eval]", script_arguments, diagnostics).map(ok)
        }
        "repl" => {
            require_single_worker(workers, "repl")?;
            repl(diagnostics).map(ok)
        }
        "install" => {
            require_single_worker(workers, "install")?;
            package_install(arguments).map(ok)
        }
        "add" => {
            require_single_worker(workers, "add")?;
            package_add(arguments).map(ok)
        }
        "remove" => {
            require_single_worker(workers, "remove")?;
            package_remove(arguments).map(ok)
        }
        "update" => {
            require_single_worker(workers, "update")?;
            package_update(arguments).map(ok)
        }
        "run" => {
            let Some(target) = arguments.first().cloned() else {
                return Err("run requires a script path or package script".into());
            };
            let script_arguments = arguments[1..].to_vec();
            let path = PathBuf::from(&target);
            if path.is_file() || looks_like_script_path(&path) {
                execute_workers(path, script_arguments, diagnostics, workers).map(ok)
            } else {
                require_single_worker(workers, "package scripts")?;
                run_package_script(&target.to_string_lossy(), &script_arguments)
            }
        }
        other => {
            let path = PathBuf::from(&command);
            if !path.is_file() && !looks_like_script_path(&path) && !other.starts_with('-') {
                // `sako dev` should run the "dev" script, the shorthand bun
                // and npm both accept. Only when no such script exists is the
                // word treated as a mistyped command.
                if package_script_exists(other) {
                    require_single_worker(workers, "package scripts")?;
                    return run_package_script(other, &arguments);
                }
                return Err(unknown_command(other));
            }
            execute_workers(path, arguments, diagnostics, workers).map(ok)
        }
    }
}

fn ok(_: ()) -> u8 {
    SUCCESS
}

fn is_global_flag(argument: &OsString) -> bool {
    let argument = argument.to_string_lossy();
    argument.starts_with("--workers=")
        || matches!(
            argument.as_ref(),
            "--memory-stats" | "--detect-leaks" | "--perf-breakdown"
        )
}

/// Whether package.json defines a script by this name. Read separately from
/// running it so a mistyped command still reports as a mistyped command rather
/// than as a missing script.
fn package_script_exists(name: &str) -> bool {
    let Ok(directory) = env::current_dir() else {
        return false;
    };
    let Ok(source) = fs::read_to_string(directory.join("package.json")) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&source) else {
        return false;
    };
    manifest
        .get("scripts")
        .and_then(|scripts| scripts.get(name))
        .and_then(serde_json::Value::as_str)
        .is_some()
}

fn unknown_command(name: &str) -> String {
    let painter = Painter::stderr();
    let mut message = format!("unknown command {}", painter.bold(name));
    if let Some(nearest) = nearest_command(name) {
        message.push_str(&format!("\n       did you mean {}?", painter.cyan(nearest)));
    }
    message.push_str(&format!(
        "\n       run {} to see what is available",
        painter.cyan("sako --help"),
    ));
    message
}

/// Suggests a command within edit distance 2, which catches the usual
/// transpositions and single missing or doubled letters without proposing
/// something unrelated for a genuinely novel word.
fn nearest_command(name: &str) -> Option<&'static str> {
    help::COMMANDS
        .iter()
        .map(|command| (command.name, edit_distance(name, command.name)))
        .filter(|(_, distance)| *distance <= 2)
        .min_by_key(|(_, distance)| *distance)
        .map(|(name, _)| name)
}

fn edit_distance(left: &str, right: &str) -> usize {
    let right_chars: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right_chars.len()).collect();
    let mut current = vec![0usize; right_chars.len() + 1];
    for (row, left_char) in left.chars().enumerate() {
        current[0] = row + 1;
        for (column, right_char) in right_chars.iter().enumerate() {
            let substitution = usize::from(left_char != *right_char);
            current[column + 1] = (previous[column] + substitution)
                .min(previous[column + 1] + 1)
                .min(current[column] + 1);
        }
        previous.clone_from(&current);
    }
    previous[right_chars.len()]
}

fn take_workers(arguments: &mut Vec<OsString>) -> Result<usize, String> {
    let Some(index) = arguments
        .iter()
        .position(|argument| argument.to_string_lossy().starts_with("--workers="))
    else {
        return Ok(1);
    };
    let argument = arguments.remove(index).to_string_lossy().into_owned();
    let value = argument.trim_start_matches("--workers=");
    let workers = if value == "auto" {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
    } else {
        value
            .parse::<usize>()
            .map_err(|_| "--workers must be a positive integer or auto".to_owned())?
    };
    if workers == 0 || workers > 256 {
        return Err("--workers must be between 1 and 256".into());
    }
    Ok(workers)
}

fn require_single_worker(workers: usize, command: &str) -> Result<(), String> {
    if workers == 1 {
        Ok(())
    } else {
        Err(format!("--workers is not supported with {command}"))
    }
}

fn execute_workers(
    path: PathBuf,
    arguments: Vec<OsString>,
    diagnostics: DiagnosticsOptions,
    workers: usize,
) -> Result<(), String> {
    if workers == 1 {
        return execute_file(&path, &arguments, diagnostics);
    }
    let mut threads = Vec::with_capacity(workers);
    for _ in 0..workers {
        let path = path.clone();
        let arguments = arguments.clone();
        threads.push(std::thread::spawn(move || {
            execute_file(&path, &arguments, diagnostics)
        }));
    }
    for thread in threads {
        thread
            .join()
            .map_err(|_| "runtime worker panicked".to_owned())??;
    }
    Ok(())
}

fn package_install(mut arguments: Vec<OsString>) -> Result<(), String> {
    let options = take_package_options(&mut arguments)?;
    if !arguments.is_empty() {
        return Err("install received an unsupported argument".into());
    }
    package_manager(options)?
        .install()
        .map_err(|error| error.to_string())
}

fn package_add(mut arguments: Vec<OsString>) -> Result<(), String> {
    let options = take_package_options(&mut arguments)?;
    let development = take_flag(&mut arguments, "--dev") || take_flag(&mut arguments, "-D");
    if arguments.is_empty() {
        return Err("add requires at least one package".into());
    }
    let mut manager = package_manager(options)?;
    for specifier in arguments {
        manager
            .add(&specifier.to_string_lossy(), development)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn package_remove(arguments: Vec<OsString>) -> Result<(), String> {
    if arguments.is_empty() {
        return Err("remove requires at least one package".into());
    }
    let mut manager = package_manager(PackageManagerOptions {
        ignore_scripts: true,
        ..PackageManagerOptions::default()
    })?;
    for name in arguments {
        manager
            .remove(&name.to_string_lossy())
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn package_update(mut arguments: Vec<OsString>) -> Result<(), String> {
    let options = take_package_options(&mut arguments)?;
    if !arguments.is_empty() {
        return Err("update received an unsupported argument".into());
    }
    package_manager(options)?
        .update()
        .map_err(|error| error.to_string())
}

/// Every package command draws the same progress bar, `remove` included: it
/// reinstalls the remaining graph and takes just as long as the rest.
fn package_manager(options: PackageManagerOptions) -> Result<PackageManager, String> {
    let root =
        env::current_dir().map_err(|error| format!("cannot read current directory: {error}"))?;
    let mut manager =
        PackageManager::new_with_options(root, options).map_err(|error| error.to_string())?;
    manager.set_reporter(Box::new(progress::Bar::new()));
    Ok(manager)
}

fn take_package_options(arguments: &mut Vec<OsString>) -> Result<PackageManagerOptions, String> {
    Ok(PackageManagerOptions {
        ignore_scripts: take_flag(arguments, "--ignore-scripts"),
        registry: take_option(arguments, "--registry")?,
        auth_token: take_option(arguments, "--token")?,
        proxy: take_option(arguments, "--proxy")?,
    })
}

fn take_option(arguments: &mut Vec<OsString>, name: &str) -> Result<Option<String>, String> {
    let prefix = format!("{name}=");
    if let Some(index) = arguments
        .iter()
        .position(|argument| argument.to_string_lossy().starts_with(&prefix))
    {
        let argument = arguments.remove(index).to_string_lossy().into_owned();
        let value = argument[prefix.len()..].to_owned();
        return (!value.is_empty())
            .then_some(Some(value))
            .ok_or_else(|| format!("{name} needs a value"));
    }
    let Some(index) = arguments.iter().position(|argument| argument == name) else {
        return Ok(None);
    };
    arguments.remove(index);
    if index >= arguments.len() {
        return Err(format!("{name} needs a value"));
    }
    let value = arguments.remove(index).to_string_lossy().into_owned();
    if value.is_empty() {
        Err(format!("{name} needs a value"))
    } else {
        Ok(Some(value))
    }
}

fn take_flag(arguments: &mut Vec<OsString>, flag: &str) -> bool {
    if let Some(index) = arguments.iter().position(|argument| argument == flag) {
        arguments.remove(index);
        true
    } else {
        false
    }
}

fn looks_like_script_path(path: &Path) -> bool {
    path.extension().is_some() || path.components().count() > 1
}

fn execute_file(
    path: &Path,
    arguments: &[OsString],
    diagnostics: DiagnosticsOptions,
) -> Result<(), String> {
    if !path.is_file() {
        return Err(format!(
            "cannot read {}: file does not exist",
            path.display()
        ));
    }
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let resource_name = path.to_string_lossy().into_owned();
    let executable = env::current_exe()
        .map_err(|error| format!("cannot locate the Sako executable: {error}"))?;
    let mut process_arguments = vec![
        executable.to_string_lossy().into_owned(),
        resource_name.clone(),
    ];
    process_arguments.extend(
        arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned()),
    );
    perf_mark(c"cli.entry-resolved");
    let mut runtime = Runtime::new().map_err(|error| error.to_string())?;
    let module = is_es_module(&path);
    perf_mark(c"cli.module-kind");
    if module {
        runtime
            .execute_module_with_args(&resource_name, &process_arguments)
            .map_err(|error| error.to_string())?;
    } else {
        runtime
            .execute_commonjs_with_args(&resource_name, &process_arguments)
            .map_err(|error| error.to_string())?;
    }
    perf_mark(c"cli.complete");
    let outcome = finish_diagnostics(&runtime, diagnostics);
    // The process exits next, so hand the isolate to the kernel rather than
    // spend the heap walk that disposing it costs. Marked because otherwise
    // whatever teardown does costs hides between `cli.complete` and the
    // wall-clock time the caller measures.
    runtime.abandon();
    perf_mark(c"runtime.teardown");
    outcome
}

fn is_es_module(path: &Path) -> bool {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("mjs" | "mts") => return true,
        Some("cjs" | "cts") => return false,
        _ => {}
    }
    for directory in path.ancestors().skip(1) {
        let manifest_path = directory.join("package.json");
        let Ok(source) = fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&source) else {
            return false;
        };
        return manifest.get("type").and_then(serde_json::Value::as_str) == Some("module");
    }
    false
}

fn execute_source(
    source: &str,
    resource_name: &str,
    arguments: Vec<String>,
    diagnostics: DiagnosticsOptions,
) -> Result<(), String> {
    let mut runtime = Runtime::new().map_err(|error| error.to_string())?;
    runtime
        .execute_with_args(source, resource_name, &arguments)
        .map_err(|error| error.to_string())?;
    finish_diagnostics(&runtime, diagnostics)
}

fn repl(diagnostics: DiagnosticsOptions) -> Result<(), String> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut runtime = Runtime::new().map_err(|error| error.to_string())?;
    let executable = env::current_exe()
        .map_err(|error| format!("cannot locate the Sako executable: {error}"))?;
    let arguments = vec![executable.to_string_lossy().into_owned()];
    let mut line = String::new();

    loop {
        print!("> ");
        io::stdout()
            .flush()
            .map_err(|error| format!("cannot write REPL prompt: {error}"))?;
        line.clear();
        if input
            .read_line(&mut line)
            .map_err(|error| format!("cannot read REPL input: {error}"))?
            == 0
        {
            break;
        }
        if matches!(line.trim(), ".exit" | ".quit") {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Err(error) = runtime.execute_with_args(&line, "[repl]", &arguments) {
            eprintln!("{error}");
        }
    }

    finish_diagnostics(&runtime, diagnostics)
}

fn run_package_script(name: &str, arguments: &[OsString]) -> Result<u8, String> {
    let directory =
        env::current_dir().map_err(|error| format!("cannot read current directory: {error}"))?;
    let manifest_path = directory.join("package.json");
    let manifest_source = fs::read_to_string(&manifest_path)
        .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_source)
        .map_err(|error| format!("invalid {}: {error}", manifest_path.display()))?;
    let scripts = manifest.get("scripts");
    let script = scripts
        .and_then(|scripts| scripts.get(name))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| undefined_script(name, scripts))?;

    let mut command_line = script.to_owned();
    for argument in arguments {
        command_line.push(' ');
        #[cfg(windows)]
        command_line.push_str(&quote_windows_argument(&argument.to_string_lossy()));
        #[cfg(unix)]
        command_line.push_str(&quote_posix_argument(&argument.to_string_lossy()));
    }

    let painter = Painter::stderr();
    eprintln!("{} {}", painter.dim("$"), painter.dim(&command_line));

    #[cfg(windows)]
    let (shell, shell_arguments) = ("cmd.exe", vec!["/d", "/s", "/c"]);
    #[cfg(unix)]
    let (shell, shell_arguments) = ("/bin/sh", vec!["-c"]);

    let status = shell_command(shell, &shell_arguments, &command_line, &directory)
        .status()
        .map_err(|error| format!("cannot run package script '{name}': {error}"))?;
    Ok(exit_code(status))
}

/// Builds the child command shared by `run` and `x`.
///
/// Two things matter here. `node_modules/.bin` goes on `PATH` so a script can
/// call a locally installed tool by bare name, which is what makes
/// `"dev": "vite"` work. And the standard streams are inherited rather than
/// captured: a dev server has to print as it runs, and previously its output
/// was buffered until the process exited -- which for a watch server is never.
fn shell_command(
    shell: &str,
    shell_arguments: &[&str],
    command_line: &str,
    directory: &Path,
) -> Command {
    let mut command = Command::new(shell);
    command
        .args(shell_arguments)
        .arg(command_line)
        .current_dir(directory)
        .env("PATH", node_bin::augmented_path(directory))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    // The shims generated by sako-package prefer this over a PATH lookup, so a
    // script re-enters the exact binary the user invoked.
    if let Ok(executable) = env::current_exe() {
        command.env("SAKO_EXECUTABLE", executable);
    }
    command
}

/// Propagates the child's own exit code, so `sako run test` is usable in CI
/// and shell `&&` chains rather than collapsing every failure to 1.
pub(crate) fn exit_code(status: std::process::ExitStatus) -> u8 {
    match status.code() {
        Some(0) => SUCCESS,
        // A u8 is all a process exit code carries; anything outside it (or a
        // signal) is
        // reported as a generic failure rather than silently truncated to 0.
        Some(code) => u8::try_from(code).unwrap_or(FAILURE),
        None => FAILURE,
    }
}

fn undefined_script(name: &str, scripts: Option<&serde_json::Value>) -> String {
    let painter = Painter::stderr();
    let mut message = format!("no script named {} in package.json", painter.bold(name));
    if let Some(available) = scripts.and_then(serde_json::Value::as_object) {
        if !available.is_empty() {
            let names: Vec<String> = available.keys().map(|key| painter.cyan(key)).collect();
            message.push_str(&format!("\n       available: {}", names.join(", ")));
        }
    }
    message
}

/// `sako x` -- run an executable from `node_modules/.bin`, like npx or bunx.
fn execute_package_binary(arguments: &[OsString]) -> Result<u8, String> {
    let Some((command, rest)) = arguments.split_first() else {
        return Err(format!(
            "x requires a command\n       {}",
            Painter::stderr().dim("example: sako x tsc --noEmit"),
        ));
    };
    let command = command.to_string_lossy().into_owned();
    let directory =
        env::current_dir().map_err(|error| format!("cannot read current directory: {error}"))?;

    let Some(executable) = node_bin::resolve(&directory, &command) else {
        return Err(missing_binary(&directory, &command));
    };

    let status = Command::new(&executable)
        .args(rest)
        .current_dir(&directory)
        .env("PATH", node_bin::augmented_path(&directory))
        .env(
            "SAKO_EXECUTABLE",
            env::current_exe().unwrap_or_else(|_| PathBuf::from("sako")),
        )
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("cannot run {}: {error}", executable.display()))?;
    Ok(exit_code(status))
}

fn missing_binary(directory: &Path, command: &str) -> String {
    let painter = Painter::stderr();
    let mut message = format!(
        "no executable named {} in node_modules",
        painter.bold(command)
    );
    if node_bin::installed_packages_providing(directory, command).is_empty() {
        message.push_str(&format!(
            "\n       install it first: {}",
            painter.cyan(&format!("sako add {command}")),
        ));
    } else {
        // The package is unpacked but has no shim, which is what an install
        // predating binary linking leaves behind.
        message.push_str(&format!(
            "\n       {} is installed but not linked; run {}",
            painter.bold(command),
            painter.cyan("sako install"),
        ));
    }
    message
}

#[cfg(windows)]
fn quote_windows_argument(argument: &str) -> String {
    format!("\"{}\"", argument.replace('"', "\"\""))
}

/// Wraps an argument in single quotes for `/bin/sh -c`, matching how npm
/// itself passes extra `run` arguments through on POSIX. Embedded single
/// quotes are closed, escaped, and reopened (`'`, `\'`, `'`), the standard
/// POSIX shell-quoting trick since a single-quoted string cannot contain one.
#[cfg(unix)]
fn quote_posix_argument(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', "'\\''"))
}

fn print_stats(stats: MemoryStats) {
    let painter = Painter::stdout();
    println!();
    print_section(painter, "  MEMORY");
    if let Ok(process) = sako_diagnostics::process_stats() {
        print_stat(painter, "RSS", &bytes(process.resident_set_bytes));
        print_stat(painter, "Private bytes", &bytes(process.private_bytes));
        print_stat(painter, "OS handles", &process.os_handles.to_string());
    }
    print_stat(painter, "V8 heap used", &bytes(stats.heap_used));
    print_stat(painter, "V8 heap committed", &bytes(stats.heap_committed));
    print_stat(painter, "V8 heap limit", &bytes(stats.heap_limit));
    print_stat(painter, "V8 external memory", &bytes(stats.external_memory));
    print_stat(painter, "Native owned", &bytes(stats.native_memory_bytes));
    println!();
    print_section(painter, "  RESOURCES");
    print_stat(
        painter,
        "Persistent handles",
        &stats.persistent_handles.to_string(),
    );
    print_stat(painter, "Timers", &stats.timers.to_string());
    print_stat(painter, "HTTP servers", &stats.http_servers.to_string());
    print_stat(painter, "Sockets", &stats.sockets.to_string());
    print_stat(painter, "HTTP buffers", &bytes(stats.http_buffer_bytes));
    print_stat(
        painter,
        "Module cache",
        &format!("{} entries", stats.module_cache_entries),
    );
    print_stat(
        painter,
        "Queued operations",
        &stats.queued_operations.to_string(),
    );
    println!();
}

fn print_stat(painter: Painter, label: &str, value: &str) {
    println!("    {label:<20}{}", painter.bold(value));
}

/// Byte counts are the main thing being read here, and raw digits past a few
/// million are hard to compare at a glance.
fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{size:.1} {} ({value})", UNITS[unit])
    }
}

fn finish_diagnostics(runtime: &Runtime, options: DiagnosticsOptions) -> Result<(), String> {
    if !options.print_memory && !options.detect_leaks {
        return Ok(());
    }
    let stats = runtime.memory_stats();
    print_stats(stats);
    if options.detect_leaks {
        let leaked = [
            (
                "persistent handles",
                stats
                    .persistent_handles
                    .saturating_sub(1 + stats.module_cache_entries),
            ),
            ("timers", stats.timers),
            ("HTTP servers", stats.http_servers),
            ("sockets", stats.sockets),
            ("HTTP buffer bytes", stats.http_buffer_bytes),
            ("queued operations", stats.queued_operations),
        ]
        .into_iter()
        .filter(|(_, count)| *count != 0)
        .map(|(name, count)| format!("{name}={count}"))
        .collect::<Vec<_>>();
        if !leaked.is_empty() {
            return Err(format!("leak check failed: {}", leaked.join(", ")));
        }
        let painter = Painter::stdout();
        println!("    {} leak check clean\n", painter.green("ok"));
    }
    Ok(())
}

fn print_section(painter: Painter, title: &str) {
    println!("{}", painter.heading(title));
}
