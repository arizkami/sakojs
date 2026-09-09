// SPDX-License-Identifier: BSD-3-Clause

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

// V8 links temporal_rs (the Temporal proposal implementation) as a set of
// `extern "C"` functions it expects to find at final-link time, but does not
// vendor them anywhere in `.deps/v8`. temporal_capi provides them, but a
// plain `cargo build` of temporal_capi silently drops every one of these
// functions from its compiled output: nothing in Rust-land calls them, and
// rustc's own reachability analysis does not know the V8 archive will need
// them by name. Declaring them here as `extern "C"` and taking their
// addresses in a `#[used]` static is what keeps them compiled in; the actual
// machine code Sako links still has to be pulled out of this crate's `.rlib`
// (and its dependencies') object files by `sako-v8`'s build script, the same
// way any other native archive member is pulled in by an unresolved symbol.
fn main() {
    let v8_root = env::var_os("SAKO_V8_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
            let workspace_root = manifest_dir
                .ancestors()
                .nth(2)
                .expect("sako-temporal-bridge must be two levels below the workspace root");
            workspace_root.join(".deps").join("v8")
        });
    let monolith = v8_root.join("lib").join("libv8_monolith.a");
    println!("cargo:rerun-if-env-changed=SAKO_V8_ROOT");
    println!("cargo:rerun-if-changed={}", monolith.display());

    let symbols = if monolith.is_file() {
        undefined_temporal_symbols(&monolith)
    } else {
        // No staged V8 yet (e.g. `cargo check` before `.deps/v8` exists):
        // emit no keepalive symbols rather than failing the build outright.
        Vec::new()
    };

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::write(out_dir.join("keepalive.rs"), render_keepalive(&symbols))
        .expect("failed to write keepalive.rs");
}

fn undefined_temporal_symbols(monolith: &std::path::Path) -> Vec<String> {
    let output = Command::new("nm")
        .arg(monolith)
        .output()
        .unwrap_or_else(|error| panic!("failed to run nm on {}: {error}", monolith.display()));
    if !output.status.success() {
        panic!(
            "nm failed on {}: {}",
            monolith.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut symbols: Vec<String> = stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let kind = parts.next()?;
            let name = parts.next()?;
            (kind == "U" && name.starts_with("temporal_rs_")).then(|| name.to_owned())
        })
        .collect();
    symbols.sort();
    symbols.dedup();
    symbols
}

fn render_keepalive(symbols: &[String]) -> String {
    let mut out = String::new();
    out.push_str("#[allow(non_snake_case)]\n");
    out.push_str("mod keepalive {\n");
    out.push_str("    unsafe extern \"C\" {\n");
    for name in symbols {
        out.push_str(&format!("        pub fn {name}();\n"));
    }
    out.push_str("    }\n");
    out.push_str(&format!(
        "    #[used]\n    static KEEP: [unsafe extern \"C\" fn(); {}] = [\n",
        symbols.len()
    ));
    for name in symbols {
        out.push_str(&format!("        {name},\n"));
    }
    out.push_str("    ];\n");
    out.push_str("}\n");
    out
}
