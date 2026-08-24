// SPDX-License-Identifier: BSD-3-Clause

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sako_package::{PackageManager, PackageManagerOptions};
use sako_process::spawn_native_with_bounded_output;
use sako_v8::{MemoryStats, Runtime};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAXIMUM_SCRIPT_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
struct DiagnosticsOptions {
    print_memory: bool,
    detect_leaks: bool,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sako: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut arguments: Vec<OsString> = env::args_os().skip(1).collect();
    let diagnostics = DiagnosticsOptions {
        print_memory: take_flag(&mut arguments, "--memory-stats"),
        detect_leaks: take_flag(&mut arguments, "--detect-leaks"),
    };
    let workers = take_workers(&mut arguments)?;
    let Some(command) = arguments.first().cloned() else {
        return Err(usage());
    };
    arguments.remove(0);

    match command.to_string_lossy().as_ref() {
        "--version" | "-V" => {
            println!("sako {VERSION}");
            Ok(())
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
            execute_source(&source, "[eval]", script_arguments, diagnostics)
        }
        "repl" => {
            require_single_worker(workers, "repl")?;
            repl(diagnostics)
        }
        "install" => {
            require_single_worker(workers, "install")?;
            package_install(arguments)
        }
        "add" => {
            require_single_worker(workers, "add")?;
            package_add(arguments)
        }
        "remove" => {
            require_single_worker(workers, "remove")?;
            package_remove(arguments)
        }
        "update" => {
            require_single_worker(workers, "update")?;
            package_update(arguments)
        }
        "run" => {
            let Some(target) = arguments.first().cloned() else {
                return Err("run requires a script path or package script".into());
            };
            let script_arguments = arguments[1..].to_vec();
            let path = PathBuf::from(&target);
            if path.is_file() || looks_like_script_path(&path) {
                execute_workers(path, script_arguments, diagnostics, workers)
            } else {
                require_single_worker(workers, "package scripts")?;
                run_package_script(&target.to_string_lossy(), &script_arguments)
            }
        }
        _ => execute_workers(PathBuf::from(command), arguments, diagnostics, workers),
    }
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

fn package_manager(options: PackageManagerOptions) -> Result<PackageManager, String> {
    let root =
        env::current_dir().map_err(|error| format!("cannot read current directory: {error}"))?;
    PackageManager::new_with_options(root, options).map_err(|error| error.to_string())
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
    let mut runtime = Runtime::new().map_err(|error| error.to_string())?;
    if is_es_module(&path) {
        runtime
            .execute_module_with_args(&resource_name, &process_arguments)
            .map_err(|error| error.to_string())?;
    } else {
        runtime
            .execute_commonjs_with_args(&resource_name, &process_arguments)
            .map_err(|error| error.to_string())?;
    }
    finish_diagnostics(&runtime, diagnostics)
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

fn run_package_script(name: &str, arguments: &[OsString]) -> Result<(), String> {
    let manifest_path = env::current_dir()
        .map_err(|error| format!("cannot read current directory: {error}"))?
        .join("package.json");
    let manifest_source = fs::read_to_string(&manifest_path)
        .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_source)
        .map_err(|error| format!("invalid {}: {error}", manifest_path.display()))?;
    let script = manifest
        .get("scripts")
        .and_then(|scripts| scripts.get(name))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("package script '{name}' is not defined"))?;

    let mut command_line = script.to_owned();
    for argument in arguments {
        command_line.push(' ');
        command_line.push_str(&quote_cmd_argument(&argument.to_string_lossy()));
    }
    let command_arguments = [
        "/d".to_owned(),
        "/s".to_owned(),
        "/c".to_owned(),
        command_line,
    ];
    let output = spawn_native_with_bounded_output(
        "cmd.exe",
        &command_arguments,
        None,
        MAXIMUM_SCRIPT_OUTPUT_BYTES,
    )
    .map_err(|error| format!("cannot run package script '{name}': {error}"))?;
    io::stdout()
        .write_all(&output.stdout)
        .map_err(|error| format!("cannot write package script stdout: {error}"))?;
    io::stderr()
        .write_all(&output.stderr)
        .map_err(|error| format!("cannot write package script stderr: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "package script '{name}' exited with {}",
            output.status
        ))
    }
}

fn quote_cmd_argument(argument: &str) -> String {
    format!("\"{}\"", argument.replace('"', "\"\""))
}

fn print_stats(stats: MemoryStats) {
    println!("Sako Memory");
    if let Ok(process) = sako_diagnostics::process_stats() {
        println!("RSS                 {} bytes", process.resident_set_bytes);
        println!("Private bytes       {} bytes", process.private_bytes);
        println!("OS handles          {}", process.os_handles);
    }
    println!("V8 heap used        {} bytes", stats.heap_used);
    println!("V8 heap committed   {} bytes", stats.heap_committed);
    println!("V8 heap limit       {} bytes", stats.heap_limit);
    println!("Persistent handles  {}", stats.persistent_handles);
    println!("Timers              {}", stats.timers);
    println!("V8 external memory  {} bytes", stats.external_memory);
    println!("HTTP servers        {}", stats.http_servers);
    println!("Sockets             {}", stats.sockets);
    println!("HTTP buffers        {} bytes", stats.http_buffer_bytes);
    println!("Native owned        {} bytes", stats.native_memory_bytes);
    println!("Module cache        {} entries", stats.module_cache_entries);
    println!("Queued operations   {}", stats.queued_operations);
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
        println!("Sako leak check    clean");
    }
    Ok(())
}

fn usage() -> String {
    "usage: sako [--memory-stats] [--detect-leaks] [--workers=N|auto] <script.js> [args...] | run <script|name> | eval <source> | repl | install | add <package> | remove <package> | update | --version".into()
}
