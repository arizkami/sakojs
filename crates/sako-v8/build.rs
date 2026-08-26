// SPDX-License-Identifier: BSD-3-Clause

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// The staged V8 is built with Chromium's hardened libc++, which is not a
/// choice we can undo: `//BUILD.gn:853` asserts `!v8_enable_sandbox ||
/// use_safe_libcxx`, and building with the sandbox off makes Torque and C++
/// disagree about `JSInterceptorMap`'s field offsets (44 vs 41), so the
/// generated static_assert fails. Sandbox on therefore implies libc++ on.
///
/// That renames every `std::` type into the `std::__Cr` inline namespace, so
/// any V8 entry point whose signature carries one — `NewDefaultPlatform`
/// returns `std::unique_ptr<Platform>` — only exists under the renamed name.
/// `cl.exe` with MSVC's STL emits calls to the un-renamed name and cannot link.
/// The fix mirrors what `build_linux` already does with `-stdlib=libc++`:
/// compile our own C++ with the same libc++ headers, staged in
/// `.deps/v8/include/libc++` together with the generated `__config_site` that
/// sets `_LIBCPP_ABI_NAMESPACE=Cr`.
fn clang_cl(v8_root: &Path) -> PathBuf {
    if let Some(path) = env::var_os("SAKO_CLANG_CL") {
        return PathBuf::from(path);
    }
    let staged = v8_root.join("toolchain").join("bin").join("clang-cl.exe");
    if staged.is_file() {
        return staged;
    }
    PathBuf::from("clang-cl.exe")
}

/// Defines that must match the flags V8 itself was compiled with. A mismatch
/// changes struct layout in the public headers and is not a link error, so
/// getting this wrong corrupts memory at runtime instead of failing the build.
/// Keep in sync with `.deps/v8/meta/args.gn`.
const V8_ABI_DEFINES: &[(&str, Option<&str>)] = &[
    ("V8_COMPRESS_POINTERS", None),
    ("V8_ENABLE_SANDBOX", None),
    (
        "_LIBCPP_HARDENING_MODE",
        Some("_LIBCPP_HARDENING_MODE_EXTENSIVE"),
    ),
    ("_LIBCPP_DISABLE_VISIBILITY_ANNOTATIONS", None),
];

/// The ICU data file, when the staged V8 needs one. A build with
/// icu_use_data_file=false has none and initializes from compiled-in data.
fn icu_data_argument(v8_root: &Path) -> Option<PathBuf> {
    let path = v8_root.join("bin").join("icudtl.dat");
    path.is_file().then_some(path)
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

    // icudtl.dat is optional: a V8 built with icu_use_data_file=false carries
    // its ICU data compiled in, which removes a 10.8 MB read (~4.6 ms) from
    // every startup. The Linux path has always treated it this way.
    validate_v8(&v8_root, "v8_monolith.lib", false);
    // Staged alongside V8 because the bridge has to be compiled with the same
    // libc++ the monolith was built with; without these the failure mode is
    // thousands of unresolved `std::__Cr::` symbols rather than a clear error.
    for path in [
        v8_root.join("lib").join("libc++.lib"),
        v8_root.join("include").join("libc++").join("__config_site"),
        v8_root
            .join("include")
            .join("libc++")
            .join("__assertion_handler"),
    ] {
        if !path.is_file() {
            panic!("required libc++ artifact is missing: {}", path.display());
        }
    }

    let include_dir = v8_root.join("include");
    let libcxx_include_dir = include_dir.join("libc++");
    let lib_dir = v8_root.join("lib");
    let v8_monolith = lib_dir.join("v8_monolith.lib");
    let libcxx_library = lib_dir.join("libc++.lib");
    let bridge = manifest_dir.join("src").join("bridge.cc");
    let napi = manifest_dir.join("src").join("napi.cc");
    let napi_include = manifest_dir.join("include");
    let bootstrap = workspace_root
        .join("runtime")
        .join("js")
        .join("bootstrap.js");
    let libraries_dir = workspace_root.join("libs");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let embedder_library = out_dir.join("v8_embedder.lib");
    let libraries_header = out_dir.join("libraries.generated.h");
    let bootstrap_header = out_dir.join("bootstrap.generated.h");
    let bootstrap_cache_header = out_dir.join("bootstrap_cache.generated.h");
    let snapshot_header = out_dir.join("snapshot.generated.h");
    let cache_generator = manifest_dir.join("src").join("bootstrap_cache.cc");
    let icu_data = icu_data_argument(&v8_root);

    create_embedder_library(&v8_monolith, &embedder_library);
    generate_bootstrap_header(&bootstrap, &bootstrap_header);
    // Before the generators: both they and the bridge library compile
    // bridge.cc, which includes this header.
    generate_libraries_header(&libraries_dir, &libraries_header);
    // Order matters: the code cache header is compiled into the snapshot
    // generator, and the snapshot header is compiled into the bridge library.
    build_and_run_generator_windows(
        &v8_root,
        &cache_generator,
        &[],
        "sako_bootstrap_cache.exe",
        &include_dir,
        &libcxx_include_dir,
        &out_dir,
        &embedder_library,
        &libcxx_library,
        icu_data.as_deref(),
        &bootstrap_cache_header,
        "bootstrap code cache",
    );
    build_and_run_generator_windows(
        &v8_root,
        &bridge,
        &["/DSAKO_SNAPSHOT_GENERATOR"],
        "sako_snapshot.exe",
        &include_dir,
        &libcxx_include_dir,
        &out_dir,
        &embedder_library,
        &libcxx_library,
        icu_data.as_deref(),
        &snapshot_header,
        "context snapshot",
    );

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .compiler(clang_cl(&v8_root))
        .std("c++20")
        .static_crt(true)
        // libc++ must precede the MSVC STL: /I directories are searched before
        // the INCLUDE environment cc sets up for the MSVC toolchain.
        .include(&libcxx_include_dir)
        .include(&include_dir)
        .include(&napi_include)
        .include(&out_dir)
        .file(&bridge)
        .file(&napi)
        .flag_if_supported("/EHsc")
        .flag_if_supported("/utf-8")
        .warnings(true);
    for (name, value) in V8_ABI_DEFINES {
        build.define(name, *value);
    }
    build.compile("sako_v8_bridge");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=v8_embedder");
    println!("cargo:rustc-link-lib=static=libc++");
    for library in [
        "advapi32", "bcrypt", "dbghelp", "kernel32", "uuid", "winmm", "shlwapi", "ole32",
        "oleaut32", "version", "ws2_32", "dnsapi", "shell32", "user32", "userenv", "delayimp",
    ] {
        println!("cargo:rustc-link-lib={library}");
    }
    println!("cargo:rustc-env=SAKO_V8_ROOT={}", v8_root.display());
    println!("cargo:rerun-if-env-changed=SAKO_V8_ROOT");
    println!("cargo:rerun-if-changed={}", bridge.display());
    println!("cargo:rerun-if-changed={}", napi.display());
    println!("cargo:rerun-if-changed={}", cache_generator.display());
    println!("cargo:rerun-if-changed={}", bootstrap.display());
    println!("cargo:rerun-if-changed={}", libraries_dir.display());
    println!("cargo:rerun-if-changed={}", v8_monolith.display());
}

fn build_linux() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir
        .ancestors()
        .nth(2)
        .expect("sako-v8 must be two levels below the workspace root");
    let v8_root = v8_root();

    validate_v8(&v8_root, "libv8_monolith.a", false);

    let include_dir = v8_root.join("include");
    let lib_dir = v8_root.join("lib");
    let bridge = manifest_dir.join("src").join("bridge.cc");
    let napi = manifest_dir.join("src").join("napi.cc");
    let napi_include = manifest_dir.join("include");
    let bootstrap = workspace_root
        .join("runtime")
        .join("js")
        .join("bootstrap.js");
    let libraries_dir = workspace_root.join("libs");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let libraries_header = out_dir.join("libraries.generated.h");
    let bootstrap_header = out_dir.join("bootstrap.generated.h");
    let bootstrap_cache_header = out_dir.join("bootstrap_cache.generated.h");
    let snapshot_header = out_dir.join("snapshot.generated.h");
    let cache_generator_source = manifest_dir.join("src").join("bootstrap_cache.cc");

    generate_bootstrap_header(&bootstrap, &bootstrap_header);
    generate_libraries_header(&libraries_dir, &libraries_header);
    // icu_use_data_file=false in this V8 build means ICU data is compiled in,
    // so there is no icudtl.dat to require; pass it through only if present.
    let icu_data = v8_root.join("bin").join("icudtl.dat");
    let icu_data = icu_data.is_file().then_some(icu_data.as_path());
    // Order matters: the code cache header is compiled into the snapshot
    // generator, and the snapshot header is compiled into the bridge library.
    build_and_run_generator_linux(
        &cache_generator_source,
        &[],
        "sako_bootstrap_cache",
        &include_dir,
        &lib_dir,
        &out_dir,
        icu_data,
        &bootstrap_cache_header,
        "bootstrap code cache",
    );
    build_and_run_generator_linux(
        &bridge,
        &["-DSAKO_SNAPSHOT_GENERATOR"],
        "sako_snapshot",
        &include_dir,
        &lib_dir,
        &out_dir,
        icu_data,
        &snapshot_header,
        "context snapshot",
    );

    // This V8 build was compiled with Chromium's custom libc++
    // (use_custom_libcxx=true), which renames std:: types into an internal
    // `std::__Cr` inline namespace. Any V8 API whose signature carries a
    // std:: type (e.g. NewDefaultPlatform's `std::unique_ptr<TracingController>`
    // parameter) exports its symbol under that renamed namespace, so linking
    // against it from a normal system-libstdc++ build fails with an undefined
    // reference. Compiling with libc++ and the same `_LIBCPP_ABI_NAMESPACE`
    // upstream libc++ added specifically for this Chromium interop case makes
    // our own std:: types mangle identically, without needing Chromium's own
    // compiled libc++ archive (unique_ptr's operations are header-only).
    cc::Build::new()
        .cpp(true)
        .compiler("clang++")
        .std("c++20")
        .flag("-stdlib=libc++")
        .define("_LIBCPP_ABI_NAMESPACE", "Cr")
        .include(&include_dir)
        .include(&napi_include)
        .include(&out_dir)
        .file(&bridge)
        .file(&napi)
        .define("V8_COMPRESS_POINTERS", None)
        .warnings(true)
        .compile("sako_v8_bridge");

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    for library in ["v8_monolith", "v8_libbase", "v8_libplatform"] {
        println!("cargo:rustc-link-lib=static={library}");
    }
    for library in ["c++", "c++abi", "dl", "pthread", "m", "rt"] {
        println!("cargo:rustc-link-lib={library}");
    }
    // An addon resolves `napi_*` against the process that loaded it, so the
    // executable has to keep those symbols in its dynamic table. Without this
    // the linker drops them as unreferenced -- nothing inside Sako calls them.
    println!("cargo:rustc-link-arg-bins=-rdynamic");
    println!("cargo:rustc-env=SAKO_V8_ROOT={}", v8_root.display());
    println!("cargo:rerun-if-env-changed=SAKO_V8_ROOT");
    println!("cargo:rerun-if-changed={}", bridge.display());
    println!("cargo:rerun-if-changed={}", napi.display());
    println!(
        "cargo:rerun-if-changed={}",
        cache_generator_source.display()
    );
    println!("cargo:rerun-if-changed={}", bootstrap.display());
    println!("cargo:rerun-if-changed={}", libraries_dir.display());
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

/// Embeds every library under `libs/` in the binary.
///
/// A library is one directory with a `src/index.ts` entry, reached from a
/// program as `sako:<directory>`. The TypeScript is embedded as written and
/// transpiled the first time something imports it, by the same parser that
/// runs a `.ts` entry file -- so there is no second copy of that parser to
/// build here, and a library nobody imports costs nothing at run time.
/// `library_sources_parse` in the test suite is what keeps a library that
/// does not parse from reaching a release.
fn generate_libraries_header(libraries_dir: &Path, output_path: &Path) {
    let mut names: Vec<String> = Vec::new();
    let mut sources: Vec<String> = Vec::new();
    let mut entries: Vec<PathBuf> = Vec::new();
    if libraries_dir.is_dir() {
        let mut directories: Vec<PathBuf> = fs::read_dir(libraries_dir)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", libraries_dir.display()))
            .map(|entry| {
                entry
                    .unwrap_or_else(|error| {
                        panic!("cannot read {}: {error}", libraries_dir.display())
                    })
                    .path()
            })
            .filter(|path| path.is_dir())
            .collect();
        // Sorted so the generated table is identical on every machine.
        directories.sort();
        for directory in directories {
            let entry = directory.join("src").join("index.ts");
            if !entry.is_file() {
                continue;
            }
            let name = directory
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_else(|| panic!("library name is not UTF-8: {}", directory.display()))
                .to_owned();
            sources.push(
                fs::read_to_string(&entry)
                    .unwrap_or_else(|error| panic!("cannot read {}: {error}", entry.display())),
            );
            names.push(name);
            entries.push(entry);
        }
    }

    let mut header = String::from(
        "// Generated from libs/*/src/index.ts.\nstruct SakoLibrarySource {\n  const char* specifier;\n  const unsigned char* source;\n  size_t length;\n};\n",
    );
    for (index, source) in sources.iter().enumerate() {
        header.push_str(&format!(
            "static constexpr unsigned char kSakoLibrarySource{index}[] = {{\n"
        ));
        for chunk in source.as_bytes().chunks(32) {
            header.push_str("  ");
            for byte in chunk {
                header.push_str(&format!("{byte},"));
            }
            header.push('\n');
        }
        header.push_str("  0,\n};\n");
    }
    header.push_str("static constexpr SakoLibrarySource kSakoLibraries[] = {\n");
    for (index, (name, source)) in names.iter().zip(sources.iter()).enumerate() {
        header.push_str(&format!(
            "  {{\"sako:{name}\", kSakoLibrarySource{index}, {}}},\n",
            source.len()
        ));
    }
    // A zero-length array is not valid C++, so an empty libs/ still needs one
    // entry for the lookup to skip.
    if names.is_empty() {
        header.push_str("  {nullptr, nullptr, 0},\n");
    }
    header.push_str("};\n");
    fs::write(output_path, header)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", output_path.display()));
    for entry in entries {
        println!("cargo:rerun-if-changed={}", entry.display());
    }
}

/// Compiles one build-time generator against the staged V8 and runs it.
///
/// Two tools share this: the bootstrap code cache producer, and the context
/// snapshot producer (which is `bridge.cc` itself, compiled with
/// `SAKO_SNAPSHOT_GENERATOR` so it grows a `main` and stubs the Rust-side
/// bindings). Both link V8 the same way, so they build the same way.
#[allow(clippy::too_many_arguments)]
fn build_and_run_generator_windows(
    v8_root: &Path,
    source: &Path,
    extra_flags: &[&str],
    executable_name: &str,
    include_dir: &Path,
    libcxx_include_dir: &Path,
    out_dir: &Path,
    embedder_library: &Path,
    libcxx_library: &Path,
    icu_data: Option<&Path>,
    output_header: &Path,
    description: &str,
) {
    let target = env::var("TARGET").unwrap();
    let generator = out_dir.join(executable_name);
    // clang-cl, not cl.exe: this links against V8's libc++-mangled symbols, so
    // it has to be built with the same standard library. See `clang_cl`.
    let mut compiler = Command::new(clang_cl(v8_root));
    // Borrow the MSVC toolchain's INCLUDE/LIB/PATH so clang-cl finds the CRT
    // and Windows SDK. Our /I flags still win: they are searched before INCLUDE.
    if let Some(tool) = cc::windows_registry::find_tool(&target, "cl.exe") {
        compiler.envs(tool.env().iter().cloned());
    }
    let status = compiler
        .current_dir(out_dir)
        .args([
            "/nologo",
            "/MT",
            "/O2",
            "/EHsc",
            "/std:c++20",
            "/utf-8",
            "-fuse-ld=lld",
        ])
        .args(extra_flags)
        .args(V8_ABI_DEFINES.iter().map(|(name, value)| match value {
            Some(value) => format!("/D{name}={value}"),
            None => format!("/D{name}"),
        }))
        .arg(format!("/I{}", libcxx_include_dir.display()))
        .arg(format!("/I{}", include_dir.display()))
        .arg(format!("/I{}", out_dir.display()))
        .arg(source)
        .arg(format!("/Fe:{}", generator.display()))
        .arg("/link")
        .arg(embedder_library)
        .arg(libcxx_library)
        .args([
            "advapi32.lib",
            "bcrypt.lib",
            "dbghelp.lib",
            "kernel32.lib",
            "uuid.lib",
            "winmm.lib",
            "shlwapi.lib",
            "ole32.lib",
            "oleaut32.lib",
            "version.lib",
            "ws2_32.lib",
            "dnsapi.lib",
            "shell32.lib",
            "user32.lib",
            "userenv.lib",
        ])
        .status()
        .unwrap_or_else(|error| panic!("failed to start clang-cl: {error}"));
    if !status.success() {
        panic!("failed to build the {description} producer");
    }
    run_generator(&generator, icu_data, output_header, description);
}

/// Same producers as the Windows path, built with the host clang++ toolchain
/// and linked directly against the staged static V8 archives. Unlike Windows
/// this needs no separate embedder-safe library step: that one works around
/// an MSVC-specific allocator-shim symbol clash that does not apply here.
#[allow(clippy::too_many_arguments)]
fn build_and_run_generator_linux(
    source: &Path,
    extra_flags: &[&str],
    executable_name: &str,
    include_dir: &Path,
    lib_dir: &Path,
    out_dir: &Path,
    icu_data: Option<&Path>,
    output_header: &Path,
    description: &str,
) {
    let generator = out_dir.join(executable_name);
    let compiler = cc::Build::new()
        .cpp(true)
        .compiler("clang++")
        .get_compiler();
    let mut command = compiler.to_command();
    command
        .arg("-std=c++20")
        .arg("-O2")
        .arg("-stdlib=libc++")
        .arg("-D_LIBCPP_ABI_NAMESPACE=Cr")
        .arg("-DV8_COMPRESS_POINTERS")
        .args(extra_flags)
        .arg(format!("-I{}", include_dir.display()))
        .arg(format!("-I{}", out_dir.display()))
        .arg(source)
        .arg("-o")
        .arg(&generator)
        .arg(format!("-L{}", lib_dir.display()))
        .arg("-lv8_monolith")
        .arg("-lv8_libbase")
        .arg("-lv8_libplatform")
        .args(["-lc++", "-lc++abi", "-ldl", "-lpthread", "-lm", "-lrt"]);
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to start the C++ compiler: {error}"));
    if !status.success() {
        panic!("failed to build the {description} producer");
    }
    run_generator(&generator, icu_data, output_header, description);
}

/// Runs a generator and insists it produced a non-empty header.
///
/// A generator that fails has to stop the build: a missing or empty artifact
/// would otherwise surface as a confusing compile error in `bridge.cc`, or
/// worse, as a runtime that silently lost the optimization it was built for.
fn run_generator(
    generator: &Path,
    icu_data: Option<&Path>,
    output_header: &Path,
    description: &str,
) {
    let status = Command::new(generator)
        // An empty argument tells the generator the ICU data is compiled in.
        .arg(icu_data.unwrap_or_else(|| Path::new("")))
        .arg(output_header)
        .status()
        .unwrap_or_else(|error| panic!("failed to run {}: {error}", generator.display()));
    if !status.success() {
        panic!("the {description} producer failed");
    }
    match fs::metadata(output_header) {
        Ok(metadata) if metadata.len() > 0 => {}
        _ => panic!(
            "the {description} producer wrote no {}",
            output_header.display()
        ),
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
