// SPDX-License-Identifier: BSD-3-Clause

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use sako_package::PackageManager;
use sako_v8::{MemoryStats, Runtime};

const VERSION: &str = env!("CARGO_PKG_VERSION");

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
    let memory_stats = take_flag(&mut arguments, "--memory-stats");
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
            let Some(source) = arguments.first() else {
                return Err("eval requires JavaScript source".into());
            };
            let source = source.to_string_lossy().into_owned();
            let script_arguments = arguments[1..]
                .iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            execute_source(&source, "[eval]", script_arguments, memory_stats)
        }
        "repl" => repl(memory_stats),
        "install" => package_install(arguments),
        "add" => package_add(arguments),
        "remove" => package_remove(arguments),
        "update" => package_update(arguments),
        "run" => {
            let Some(target) = arguments.first().cloned() else {
                return Err("run requires a script path or package script".into());
            };
            let script_arguments = arguments[1..].to_vec();
            let path = PathBuf::from(&target);
            if path.is_file() || looks_like_script_path(&path) {
                execute_file(&path, &script_arguments, memory_stats)
            } else {
                run_package_script(&target.to_string_lossy(), &script_arguments)
            }
        }
        _ => execute_file(&PathBuf::from(command), &arguments, memory_stats),
    }
}

fn package_install(mut arguments: Vec<OsString>) -> Result<(), String> {
    let ignore_scripts = take_flag(&mut arguments, "--ignore-scripts");
    if !arguments.is_empty() {
        return Err("install only accepts --ignore-scripts".into());
    }
    package_manager(ignore_scripts)?
        .install()
        .map_err(|error| error.to_string())
}

fn package_add(mut arguments: Vec<OsString>) -> Result<(), String> {
    let ignore_scripts = take_flag(&mut arguments, "--ignore-scripts");
    let development = take_flag(&mut arguments, "--dev") || take_flag(&mut arguments, "-D");
    if arguments.is_empty() {
        return Err("add requires at least one package".into());
    }
    let mut manager = package_manager(ignore_scripts)?;
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
    let mut manager = package_manager(true)?;
    for name in arguments {
        manager
            .remove(&name.to_string_lossy())
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn package_update(mut arguments: Vec<OsString>) -> Result<(), String> {
    let ignore_scripts = take_flag(&mut arguments, "--ignore-scripts");
    if !arguments.is_empty() {
        return Err("update only accepts --ignore-scripts".into());
    }
    package_manager(ignore_scripts)?
        .update()
        .map_err(|error| error.to_string())
}

fn package_manager(ignore_scripts: bool) -> Result<PackageManager, String> {
    let root =
        env::current_dir().map_err(|error| format!("cannot read current directory: {error}"))?;
    PackageManager::new(root, ignore_scripts).map_err(|error| error.to_string())
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

fn execute_file(path: &Path, arguments: &[OsString], memory_stats: bool) -> Result<(), String> {
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
    if memory_stats {
        print_stats(runtime.memory_stats());
    }
    Ok(())
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
    print_memory_stats: bool,
) -> Result<(), String> {
    let mut runtime = Runtime::new().map_err(|error| error.to_string())?;
    runtime
        .execute_with_args(source, resource_name, &arguments)
        .map_err(|error| error.to_string())?;
    if print_memory_stats {
        print_stats(runtime.memory_stats());
    }
    Ok(())
}

fn repl(print_memory_stats: bool) -> Result<(), String> {
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

    if print_memory_stats {
        print_stats(runtime.memory_stats());
    }
    Ok(())
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
    let status = Command::new("cmd.exe")
        .args(["/d", "/s", "/c", &command_line])
        .status()
        .map_err(|error| format!("cannot start package script '{name}': {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("package script '{name}' exited with {status}"))
    }
}

fn quote_cmd_argument(argument: &str) -> String {
    format!("\"{}\"", argument.replace('"', "\"\""))
}

fn print_stats(stats: MemoryStats) {
    println!("Sako Memory");
    println!("V8 heap used        {} bytes", stats.heap_used);
    println!("V8 heap committed   {} bytes", stats.heap_committed);
    println!("V8 heap limit       {} bytes", stats.heap_limit);
    println!("Persistent handles  {}", stats.persistent_handles);
    println!("Timers              {}", stats.timers);
}

fn usage() -> String {
    "usage: sako [--memory-stats] <script.js> [args...] | run <script|name> | eval <source> | repl | install | add <package> | remove <package> | update | --version".into()
}
