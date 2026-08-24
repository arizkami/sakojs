// SPDX-License-Identifier: BSD-3-Clause

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sako_v8::Runtime;

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
    let mut args = env::args_os();
    let _executable = args.next();
    let Some(first) = args.next() else {
        return Err("usage: sako <script.js>".into());
    };

    if args.next().is_some() {
        return Err("script arguments are not supported yet".into());
    }

    let script_path = PathBuf::from(first);
    execute_file(&script_path)
}

fn execute_file(path: &Path) -> Result<(), String> {
    let source = fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let resource_name = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned();

    let mut runtime = Runtime::new().map_err(|error| error.to_string())?;
    runtime
        .execute(&source, &resource_name)
        .map_err(|error| error.to_string())
}
