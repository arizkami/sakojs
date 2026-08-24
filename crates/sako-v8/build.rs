// SPDX-License-Identifier: BSD-3-Clause

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const REQUIRED_V8_MAJOR: &str = "14";
const MALLOC_SHIM_MEMBER: &str = "obj/third_party/partition_alloc/src/partition_alloc/allocator_shim/allocator_shim_win_static.obj";

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        panic!("Sako.js Phase 1 supports only Windows");
    }
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64") {
        panic!("Sako.js Phase 1 requires Windows x86_64");
    }
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        panic!("the supplied V8 build requires the MSVC ABI");
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir
        .ancestors()
        .nth(2)
        .expect("sako-v8 must be two levels below the workspace root");
    let v8_root = env::var_os("SAKO_V8_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join(".deps").join("v8"));

    validate_v8(&v8_root);

    let include_dir = v8_root.join("include");
    let lib_dir = v8_root.join("lib");
    let v8_monolith = lib_dir.join("v8_monolith.lib");
    let bridge = manifest_dir.join("src").join("bridge.cc");
    let bootstrap = workspace_root
        .join("runtime")
        .join("js")
        .join("bootstrap.js");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let embedder_library = out_dir.join("v8_embedder.lib");
    let bootstrap_header = out_dir.join("bootstrap.generated.h");

    create_embedder_library(&v8_monolith, &embedder_library);
    generate_bootstrap_header(&bootstrap, &bootstrap_header);

    cc::Build::new()
        .cpp(true)
        .std("c++20")
        .static_crt(true)
        .include(&include_dir)
        .include(&out_dir)
        .file(&bridge)
        .define("V8_COMPRESS_POINTERS", None)
        .flag_if_supported("/EHsc")
        .flag_if_supported("/Zc:__cplusplus")
        .flag_if_supported("/utf-8")
        .warnings(true)
        .compile("sako_v8_bridge");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=v8_embedder");
    for library in ["advapi32", "bcrypt", "dbghelp", "kernel32", "uuid", "winmm"] {
        println!("cargo:rustc-link-lib={library}");
    }
    println!("cargo:rustc-env=SAKO_V8_ROOT={}", v8_root.display());
    println!("cargo:rerun-if-env-changed=SAKO_V8_ROOT");
    println!("cargo:rerun-if-changed={}", bridge.display());
    println!("cargo:rerun-if-changed={}", bootstrap.display());
    println!("cargo:rerun-if-changed={}", v8_monolith.display());
}

fn generate_bootstrap_header(source_path: &Path, output_path: &Path) {
    let source = fs::read_to_string(source_path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", source_path.display()));
    const DELIMITER: &str = "SAKO_BOOT_7D9C";
    assert!(
        !source.contains(DELIMITER),
        "bootstrap contains raw-string delimiter"
    );
    let header = format!(
        "// Generated from runtime/js/bootstrap.js.\nstatic constexpr const char kSakoBootstrap[] = R\"{DELIMITER}({source}){DELIMITER}\";\n"
    );
    fs::write(output_path, header)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", output_path.display()));
}

fn create_embedder_library(source: &Path, output: &Path) {
    let target = env::var("TARGET").unwrap();
    let mut librarian =
        cc::windows_registry::find(&target, "lib.exe").unwrap_or_else(|| Command::new("lib.exe"));
    let status = librarian
        .arg("/nologo")
        .arg(format!("/out:{}", output.display()))
        .arg(format!("/remove:{MALLOC_SHIM_MEMBER}"))
        .arg(source)
        .status()
        .unwrap_or_else(|error| panic!("failed to start the MSVC librarian: {error}"));
    if !status.success() {
        panic!(
            "failed to create embedder-safe V8 library from {}",
            source.display()
        );
    }
}

fn validate_v8(root: &Path) {
    let required = [
        root.join("include").join("v8.h"),
        root.join("include")
            .join("libplatform")
            .join("libplatform.h"),
        root.join("lib").join("v8_monolith.lib"),
        root.join("bin").join("icudtl.dat"),
    ];
    for path in required {
        if !path.is_file() {
            panic!("required V8 artifact is missing: {}", path.display());
        }
    }

    let version_header = root.join("include").join("v8-version.h");
    let version = fs::read_to_string(&version_header)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", version_header.display()));
    let expected = format!("#define V8_MAJOR_VERSION {REQUIRED_V8_MAJOR}");
    if !version.lines().any(|line| line.trim() == expected) {
        panic!(
            "unsupported V8 headers in {}: Phase 1 expects V8 major version {}",
            root.display(),
            REQUIRED_V8_MAJOR
        );
    }
}
