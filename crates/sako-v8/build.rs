// SPDX-License-Identifier: BSD-3-Clause

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

include!("build/linux_link.rs");

const REQUIRED_V8_MAJOR: &str = "15";
const MALLOC_SHIM_MEMBER: &str = "obj/third_party/partition_alloc/src/partition_alloc/allocator_shim/allocator_shim_win_static.obj";

fn main() {
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64") {
        panic!("Sako.js requires an x86_64 target");
    }
    match env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("windows") => build_windows(),
        Ok("linux") => build_linux(),
        _ => panic!("Sako.js supports only Windows and Linux"),
    }
}

fn v8_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir
        .ancestors()
        .nth(2)
        .expect("sako-v8 must be two levels below the workspace root");
    env::var_os("SAKO_V8_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join(".deps").join("v8"))
}

fn build_windows() {
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        panic!("the supplied V8 build requires the MSVC ABI");
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir
        .ancestors()
        .nth(2)
        .expect("sako-v8 must be two levels below the workspace root");
    let v8_root = v8_root();

    validate_v8(&v8_root, "v8_monolith.lib", true);

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
    let bootstrap_cache_header = out_dir.join("bootstrap_cache.generated.h");
    let cache_generator = manifest_dir.join("src").join("bootstrap_cache.cc");

    create_embedder_library(&v8_monolith, &embedder_library);
    generate_bootstrap_header(&bootstrap, &bootstrap_header);
    generate_bootstrap_cache_windows(
        &cache_generator,
        &include_dir,
        &out_dir,
        &embedder_library,
        &v8_root.join("bin").join("icudtl.dat"),
        &bootstrap_cache_header,
    );

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
    println!("cargo:rerun-if-changed={}", cache_generator.display());
    println!("cargo:rerun-if-changed={}", bootstrap.display());
    println!("cargo:rerun-if-changed={}", v8_monolith.display());
}

fn build_linux() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir
        .ancestors()
        .nth(2)
        .expect("sako-v8 must be two levels below the workspace root")
        .to_path_buf();
    let v8_root = v8_root();

    validate_v8(&v8_root, "libv8_monolith.a", false);

    let include_dir = v8_root.join("include");
    let lib_dir = v8_root.join("lib");
    let bridge = manifest_dir.join("src").join("bridge.cc");
    let bootstrap = workspace_root
        .join("runtime")
        .join("js")
        .join("bootstrap.js");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let bootstrap_header = out_dir.join("bootstrap.generated.h");
    let bootstrap_cache_header = out_dir.join("bootstrap_cache.generated.h");
    let cache_generator_source = manifest_dir.join("src").join("bootstrap_cache.cc");

    let libcxx = libcxx_root(&workspace_root);
    validate_libcxx(&libcxx);
    let libcxx_include = libcxx.join("include").join("c++").join("v1");
    let libcxx_lib = libcxx.join("lib");

    // `.deps/v8/lib/libv8_libbase.a` and `libv8_libplatform.a` are thin
    // archives whose members reference object files that were never staged
    // (see docs/v8-linkage.md); `libv8_monolith.a` alone already contains
    // everything they would provide, so only that one is linked.
    //
    // `string.cpp.o` is extracted from our libc++.a and force-included as a
    // plain object alongside the normal `-lc++` archive link: `lld` fails to
    // resolve four `basic_string<char>` members V8 needs
    // (`__assign_external`, `__erase_external_with_move`,
    // `__init_copy_ctor_external`) purely from the archive, even though `nm`
    // proves the symbols are there — a reproducible `lld` archive-vs-archive
    // resolution quirk, not an ABI mismatch. See docs/v8-linkage.md.
    let force_string_o = force_libcxx_string_object(&libcxx_lib, &out_dir);

    // V8 15's Temporal support calls into `temporal_capi` (a Rust crate)
    // unconditionally from every isolate, but `.deps/v8` doesn't vendor it.
    // `sako-temporal-bridge` compiles it; the actual machine code has to be
    // pulled out of that crate's (and its dependencies') `.rlib` object
    // files here, the same way any other archive member is pulled in by an
    // unresolved symbol — seeing `sako-temporal-bridge` as a normal Cargo
    // dependency is not enough on its own. See docs/v8-linkage.md.
    let mut temporal_objects = temporal_bridge_objects(&out_dir);
    temporal_objects.extend(sysroot_objects(&out_dir));

    generate_bootstrap_header(&bootstrap, &bootstrap_header);
    // icu_use_data_file=false in this V8 build means ICU data is compiled in,
    // so there is no icudtl.dat to require; pass it through only if present.
    let icu_data = v8_root.join("bin").join("icudtl.dat");
    let icu_data = icu_data.is_file().then_some(icu_data.as_path());
    generate_bootstrap_cache_linux(
        &cache_generator_source,
        &include_dir,
        &libcxx_include,
        &libcxx_lib,
        &lib_dir,
        &force_string_o,
        &temporal_objects,
        &out_dir,
        icu_data,
        &bootstrap_cache_header,
    );

    // This V8 build was compiled with Chromium's custom libc++
    // (use_custom_libcxx=true, ABI namespace `__Cr`, ABI version 2), which
    // renames and relayouts std:: types relative to upstream libc++. Rather
    // than fighting the system libc++'s headers into that ABI at compile
    // time, `libcxx` above points at a libc++ we build once from upstream
    // LLVM with `-DLIBCXX_ABI_NAMESPACE=__Cr -DLIBCXX_ABI_VERSION=2`, which
    // is enough to match this V8 build exactly. See docs/v8-linkage.md.
    cc::Build::new()
        .cpp(true)
        .compiler("clang++")
        .std("c++20")
        .flag("-nostdinc++")
        .flag(format!("-isystem{}", libcxx_include.display()))
        .include(&include_dir)
        .include(&out_dir)
        .file(&bridge)
        .define("V8_COMPRESS_POINTERS", None)
        .define("V8_ENABLE_SANDBOX", None)
        .warnings(true)
        .compile("sako_v8_bridge");

    // `cargo:rustc-link-arg` only applies to targets built *within this
    // package* — it does not propagate to a downstream binary that merely
    // depends on sako-v8 (unlike `cargo:rustc-link-lib`/`-search`, this was
    // confirmed empirically: emitting these here never reached sako-cli's
    // actual link line, even though Cargo faithfully recorded them). So
    // these directives are still needed here, for sako-v8's *own*
    // integration tests/examples (which are "within this package" and do
    // get them) — but `sako-cli` additionally duplicates this whole
    // sequence in its own build script (via `build/linux_link.rs`) for the
    // real, final `sako` binary. See docs/v8-linkage.md. `cargo:root` is
    // how that build script finds the `sako_v8_bridge` static library this
    // one produces, per the usual convention for a `links`-owning build
    // script exposing its `OUT_DIR`.
    println!("cargo:root={}", out_dir.display());
    println!("cargo:rustc-link-arg=-fuse-ld=lld");
    println!("cargo:rustc-link-arg=-Wl,--allow-multiple-definition");
    println!("cargo:rustc-link-arg=-Wl,--start-group");
    println!("cargo:rustc-link-arg=-L{}", lib_dir.display());
    println!("cargo:rustc-link-arg=-lv8_monolith");
    println!("cargo:rustc-link-arg={}", force_string_o.display());
    for object in &temporal_objects {
        println!("cargo:rustc-link-arg={}", object.display());
    }
    println!("cargo:rustc-link-arg=-Wl,--end-group");
    // Statically link our libc++ so the binary can't pick up the system
    // libc++.so (a different, incompatible ABI) at load time instead.
    println!("cargo:rustc-link-arg=-Wl,-Bstatic");
    println!("cargo:rustc-link-arg=-L{}", libcxx_lib.display());
    for library in ["-lc++", "-lc++abi", "-lunwind"] {
        println!("cargo:rustc-link-arg={library}");
    }
    println!("cargo:rustc-link-arg=-Wl,-Bdynamic");
    for library in ["dl", "pthread", "m", "rt"] {
        println!("cargo:rustc-link-lib={library}");
    }
    println!("cargo:rustc-env=SAKO_V8_ROOT={}", v8_root.display());
    println!("cargo:rerun-if-env-changed=SAKO_V8_ROOT");
    println!("cargo:rerun-if-env-changed=SAKO_LIBCXX_ROOT");
    println!("cargo:rerun-if-changed={}", bridge.display());
    println!("cargo:rerun-if-changed={}", cache_generator_source.display());
    println!("cargo:rerun-if-changed={}", bootstrap.display());
    println!(
        "cargo:rerun-if-changed={}",
        lib_dir.join("libv8_monolith.a").display()
    );
}

fn generate_bootstrap_header(source_path: &Path, output_path: &Path) {
    let source = fs::read_to_string(source_path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", source_path.display()));
    let mut header = String::from(
        "// Generated from runtime/js/bootstrap.js.\nstatic constexpr unsigned char kSakoBootstrap[] = {\n",
    );
    for chunk in source.as_bytes().chunks(32) {
        header.push_str("  ");
        for byte in chunk {
            header.push_str(&format!("{byte},"));
        }
        header.push('\n');
    }
    header.push_str("  0,\n};\n");
    fs::write(output_path, header)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", output_path.display()));
}

// Compiles and runs the code cache producer so the bridge can embed V8's
// compiled form of the bootstrap instead of parsing the source on every run.
fn generate_bootstrap_cache_windows(
    source: &Path,
    include_dir: &Path,
    out_dir: &Path,
    embedder_library: &Path,
    icu_data: &Path,
    output_header: &Path,
) {
    let target = env::var("TARGET").unwrap();
    let generator = out_dir.join("sako_bootstrap_cache.exe");
    let mut compiler = cc::windows_registry::find(&target, "cl.exe")
        .unwrap_or_else(|| Command::new("cl.exe"));
    let status = compiler
        .current_dir(out_dir)
        .args([
            "/nologo",
            "/MT",
            "/O2",
            "/EHsc",
            "/std:c++20",
            "/Zc:__cplusplus",
            "/utf-8",
        ])
        .arg("/DV8_COMPRESS_POINTERS")
        .arg(format!("/I{}", include_dir.display()))
        .arg(format!("/I{}", out_dir.display()))
        .arg(source)
        .arg(format!("/Fe:{}", generator.display()))
        .arg("/link")
        .arg(embedder_library)
        .args([
            "advapi32.lib",
            "bcrypt.lib",
            "dbghelp.lib",
            "kernel32.lib",
            "uuid.lib",
            "winmm.lib",
        ])
        .status()
        .unwrap_or_else(|error| panic!("failed to start the MSVC compiler: {error}"));
    if !status.success() {
        panic!("failed to build the bootstrap code cache producer");
    }
    let status = Command::new(&generator)
        .arg(icu_data)
        .arg(output_header)
        .status()
        .unwrap_or_else(|error| {
            panic!("failed to run {}: {error}", generator.display())
        });
    if !status.success() {
        panic!("the bootstrap code cache producer failed");
    }
}

/// Same producer as the Windows path, built with the host g++/clang++
/// toolchain and linked directly against the staged static V8 archives.
/// Unlike Windows this needs no separate embedder-safe library step: that
/// one works around an MSVC-specific allocator-shim symbol clash that does
/// not apply to the GNU/Clang toolchain.
fn generate_bootstrap_cache_linux(
    source: &Path,
    include_dir: &Path,
    libcxx_include: &Path,
    libcxx_lib: &Path,
    lib_dir: &Path,
    force_string_o: &Path,
    temporal_objects: &[PathBuf],
    out_dir: &Path,
    icu_data: Option<&Path>,
    output_header: &Path,
) {
    let generator = out_dir.join("sako_bootstrap_cache");
    let compiler = cc::Build::new().cpp(true).compiler("clang++").get_compiler();
    let mut command = compiler.to_command();
    command
        .arg("-std=c++20")
        .arg("-O2")
        .arg("-fuse-ld=lld")
        .arg("-nostdinc++")
        .arg(format!("-isystem{}", libcxx_include.display()))
        .arg("-DV8_COMPRESS_POINTERS")
        .arg("-DV8_ENABLE_SANDBOX")
        .arg(format!("-I{}", include_dir.display()))
        .arg(format!("-I{}", out_dir.display()))
        .arg(source)
        .arg("-o")
        .arg(&generator)
        .arg("-Wl,--start-group")
        .arg(format!("-L{}", lib_dir.display()))
        .arg("-lv8_monolith")
        .arg(force_string_o)
        .args(temporal_objects)
        .arg("-Wl,--end-group")
        .arg("-Wl,-Bstatic")
        .arg(format!("-L{}", libcxx_lib.display()))
        .args(["-lc++", "-lc++abi", "-lunwind"])
        .arg("-Wl,-Bdynamic")
        .args(["-ldl", "-lpthread", "-lm", "-lrt"]);
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to start the C++ compiler: {error}"));
    if !status.success() {
        panic!("failed to build the bootstrap code cache producer");
    }
    let status = Command::new(&generator)
        .arg(icu_data.unwrap_or_else(|| Path::new("")))
        .arg(output_header)
        .status()
        .unwrap_or_else(|error| panic!("failed to run {}: {error}", generator.display()));
    if !status.success() {
        panic!("the bootstrap code cache producer failed");
    }
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

fn validate_v8(root: &Path, monolith_name: &str, require_icu_data: bool) {
    let mut required = vec![
        root.join("include").join("v8.h"),
        root.join("include")
            .join("libplatform")
            .join("libplatform.h"),
        root.join("lib").join(monolith_name),
    ];
    if require_icu_data {
        required.push(root.join("bin").join("icudtl.dat"));
    }
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
            "unsupported V8 headers in {}: Sako.js expects V8 major version {}",
            root.display(),
            REQUIRED_V8_MAJOR
        );
    }
}
