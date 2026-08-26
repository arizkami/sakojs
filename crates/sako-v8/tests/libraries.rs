// SPDX-License-Identifier: BSD-3-Clause

//! The `libs/` tree is embedded in the binary as TypeScript and transpiled the
//! first time a program imports it. Nothing in the build parses it, so this is
//! what stops a library that does not parse from reaching a release: the
//! failure would otherwise surface as an error inside whichever program first
//! wrote `import ... from "sako:<name>"`.

use std::path::{Path, PathBuf};

use sako_typescript::{OutputModuleKind, transpile};

fn libraries_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("libs")
}

fn entries() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(libraries_dir())
        .expect("libs/ should be readable")
        .map(|entry| entry.expect("libs/ entry should be readable").path())
        .filter(|path| path.is_dir())
        .map(|path| path.join("src").join("index.ts"))
        .filter(|path| path.is_file())
        .collect();
    found.sort();
    found
}

#[test]
fn library_sources_parse() {
    let entries = entries();
    assert!(
        !entries.is_empty(),
        "libs/ should hold at least one library"
    );
    for entry in entries {
        let source = std::fs::read_to_string(&entry)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", entry.display()));
        let emitted = transpile(&entry, &source, OutputModuleKind::Esm)
            .unwrap_or_else(|error| panic!("{} does not build: {error}", entry.display()));
        assert!(
            !emitted.trim().is_empty(),
            "{} transpiled to nothing",
            entry.display()
        );
    }
}

/// Every library has to be importable as `sako:<directory>`, so a name that
/// needs quoting or escaping in a specifier is a name the loader cannot serve.
#[test]
fn library_names_are_plain_specifiers() {
    for entry in entries() {
        let name = entry
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .expect("a library directory should have a UTF-8 name");
        assert!(
            !name.is_empty()
                && name
                    .chars()
                    .all(|character| character.is_ascii_lowercase() || character == '-'),
            "library name is not a plain specifier: {name}"
        );
    }
}
