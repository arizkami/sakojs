// SPDX-License-Identifier: BSD-3-Clause
//
// Shared between `sako-v8/build.rs` and `sako-cli/build.rs` via `include!`
// (see the comment at each call site for why sako-cli's build script needs
// its own copy of this logic rather than reusing directives sako-v8's
// build script already emitted). Expects `std::{env, fs}`,
// `std::path::{Path, PathBuf}`, and `std::process::Command` already in
// scope from the including file.

/// `.deps/v8` is a staged prebuilt artifact this repository does not build;
/// the custom-ABI libc++ it needs to link against on Linux is the same kind
/// of prerequisite, staged the same way. See docs/v8-linkage.md for the
/// exact recipe to produce one.
fn libcxx_root(workspace_root: &Path) -> PathBuf {
    env::var_os("SAKO_LIBCXX_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join(".deps").join("libcxx-cr"))
}

fn validate_libcxx(root: &Path) {
    for relative in [
        Path::new("include").join("c++").join("v1").join("string"),
        Path::new("lib").join("libc++.a"),
        Path::new("lib").join("libc++abi.a"),
        Path::new("lib").join("libunwind.a"),
    ] {
        let path = root.join(&relative);
        if !path.is_file() {
            panic!(
                "required libc++ artifact is missing: {} (see docs/v8-linkage.md \
                 for how to build one, or set SAKO_LIBCXX_ROOT)",
                path.display()
            );
        }
    }
}

/// Extracts `string.cpp.o` from our libc++.a so it can be force-included on
/// the final link line; see docs/v8-linkage.md for why.
fn force_libcxx_string_object(libcxx_lib: &Path, out_dir: &Path) -> PathBuf {
    let archive = libcxx_lib.join("libc++.a");
    let dest_dir = out_dir.join("libcxx_force");
    fs::create_dir_all(&dest_dir).expect("failed to create libcxx_force directory");
    let status = Command::new("llvm-ar")
        .arg("x")
        .arg(&archive)
        .arg("string.cpp.o")
        .current_dir(&dest_dir)
        .status()
        .unwrap_or_else(|error| panic!("failed to start llvm-ar: {error}"));
    if !status.success() {
        panic!("failed to extract string.cpp.o from {}", archive.display());
    }
    dest_dir.join("string.cpp.o")
}

/// Crates whose compiled object code V8's Temporal builtins need at link
/// time (transitive runtime dependencies of `sako-temporal-bridge`, minus
/// proc-macro/build-only crates that never appear in the final binary).
const TEMPORAL_BRIDGE_CRATES: &[&str] = &[
    "sako_temporal_bridge",
    "temporal_capi",
    "temporal_rs",
    "icu_calendar",
    "icu_calendar_data",
    "icu_locale_core",
    "icu_locale_fallback",
    "icu_locale_fallback_data",
    "icu_provider",
    "num_traits",
    "timezone_provider",
    "writeable",
    "zoneinfo64",
    "diplomat_runtime",
    "resb",
    "calendrical_calculations",
    "tinystr",
    "zerovec",
    "zerotrie",
    "potential_utf",
    "litemap",
    "yoke",
    "zerofrom",
    "smallvec",
    "ixdtf",
    "stable_deref_trait",
    "strck",
    "core_maths",
    "unicode_ident",
    "serde",
    "serde_core",
    "libm",
];

/// Needed only by `sako-v8`'s build script, for its standalone C++
/// `bootstrap_cache` tool (not a normal Rust binary, so nothing links
/// std/core/alloc into it automatically the way rustc does for `sako-cli`
/// itself) — unused, and so `#[allow(dead_code)]`, when this file is
/// `include!`'d from `sako-cli/build.rs` instead. Rust's own sysroot rlibs'
/// generic/monomorphized symbols are weak (ODR-style), so pulling them in
/// alongside `sako-temporal-bridge`'s own bundled subset doesn't conflict
/// with it — only genuinely distinct code (like the allocator shim, which
/// only `sako-temporal-bridge` defines) would, and nothing else here
/// defines that.
#[allow(dead_code)]
const SYSROOT_CRATES: &[&str] = &[
    "std",
    "core",
    "alloc",
    // Not "panic_unwind": the workspace profile sets `panic = "abort"`
    // (see the root Cargo.toml), and the two panic runtimes both define
    // `__rust_start_panic`/`__rust_panic_cleanup` — only one may be linked.
    "panic_abort",
    "compiler_builtins",
    "unwind",
    "std_detect",
    "addr2line",
    "gimli",
    "object",
    "memchr",
    "rustc_demangle",
    "hashbrown",
    "miniz_oxide",
    "adler2",
    "cfg_if",
    "rustc_std_workspace_core",
    "rustc_std_workspace_alloc",
];

#[allow(dead_code)]
fn sysroot_lib_dir() -> PathBuf {
    let sysroot = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("--print")
        .arg("sysroot")
        .output()
        .unwrap_or_else(|error| panic!("failed to run rustc --print sysroot: {error}"));
    if !sysroot.status.success() {
        panic!(
            "rustc --print sysroot failed: {}",
            String::from_utf8_lossy(&sysroot.stderr)
        );
    }
    let sysroot = String::from_utf8(sysroot.stdout)
        .expect("rustc --print sysroot did not print UTF-8")
        .trim()
        .to_owned();
    let target = env::var("TARGET").expect("Cargo always sets TARGET for build scripts");
    PathBuf::from(sysroot)
        .join("lib")
        .join("rustlib")
        .join(target)
        .join("lib")
}

/// Extracts every object file belonging to `SYSROOT_CRATES` out of their
/// `.rlib`s in the active Rust toolchain's sysroot.
#[allow(dead_code)]
fn sysroot_objects(out_dir: &Path) -> Vec<PathBuf> {
    let lib_dir = sysroot_lib_dir();
    let mut objects = Vec::new();
    for &crate_name in SYSROOT_CRATES {
        let rlib = fs::read_dir(&lib_dir)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", lib_dir.display()))
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_prefix("lib"))
                    .and_then(|name| name.strip_suffix(".rlib"))
                    .and_then(|stem| stem.rsplit_once('-'))
                    .is_some_and(|(name, _hash)| name == crate_name)
            })
            .unwrap_or_else(|| {
                panic!(
                    "sysroot crate '{crate_name}' was not found in {}",
                    lib_dir.display()
                )
            });
        let dest_dir = out_dir.join("sysroot_objs").join(crate_name);
        // Clear any objects extracted here by a previous build (e.g. from a
        // now-stale hashed rlib) so they can't linger and cause duplicate
        // symbols against what we're about to extract.
        let _ = fs::remove_dir_all(&dest_dir);
        fs::create_dir_all(&dest_dir)
            .unwrap_or_else(|error| panic!("failed to create {}: {error}", dest_dir.display()));
        let status = Command::new("llvm-ar")
            .arg("x")
            .arg(&rlib)
            .current_dir(&dest_dir)
            .status()
            .unwrap_or_else(|error| panic!("failed to start llvm-ar: {error}"));
        if !status.success() {
            panic!("failed to extract objects from {}", rlib.display());
        }
        for object in fs::read_dir(&dest_dir)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", dest_dir.display()))
        {
            let object = object.expect("failed to read an extracted object entry");
            if object.path().extension().and_then(|ext| ext.to_str()) == Some("o") {
                objects.push(object.path());
            }
        }
    }
    objects
}

/// Runs `nm` on an `.rlib`/`.a` and returns its defined (`T`/`W`/`t`/`w`)
/// and undefined (`U`) symbol names.
fn nm_symbols(
    archive: &Path,
) -> (
    std::collections::HashSet<String>,
    std::collections::HashSet<String>,
) {
    let output = Command::new("nm")
        .arg(archive)
        .output()
        .unwrap_or_else(|error| panic!("failed to run nm on {}: {error}", archive.display()));
    let mut defined = std::collections::HashSet::new();
    let mut undefined = std::collections::HashSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        // Defined symbols print as `<address> <kind> <name>`; undefined
        // ones omit the address: `                 <kind> <name>`.
        let parts: Vec<&str> = line.split_whitespace().collect();
        let (kind, name) = match parts.as_slice() {
            [_address, kind, name] => (*kind, *name),
            [kind, name] => (*kind, *name),
            _ => continue,
        };
        match kind {
            "U" => {
                undefined.insert(name.to_owned());
            }
            "T" | "W" | "t" | "w" => {
                defined.insert(name.to_owned());
            }
            _ => {}
        }
    }
    (defined, undefined)
}

/// Rust v0-mangles every ordinary item name as `cratename::path::to::item`
/// (crate-qualified, and folding in the defining crate's own metadata
/// hash), so two differently-hashed compiles of the *same* crate never
/// produce colliding *defined* symbol names for its ordinary Rust items.
/// Two kinds of defined symbol are the exception, and force picking exactly
/// one candidate for the crate that has any:
/// - Genuinely unmangled `#[no_mangle]` names — diplomat's generated
///   `extern "C"` shims, `diplomat_runtime`'s own C-ABI helpers — kept
///   fixed and flat on purpose, for FFI.
/// - `#[rustc_std_internal_symbol]` items (the global allocator shim,
///   panic-runtime hooks) — still v0-mangled, but rustc deliberately
///   demangles these as `__rustc::name` regardless of which crate defines
///   them, since only one definition may ever exist in a given binary.
fn has_unstable_defined_symbol(archive: &Path) -> bool {
    let output = Command::new("nm")
        .arg("-C")
        .arg(archive)
        .output()
        .unwrap_or_else(|error| panic!("failed to run nm on {}: {error}", archive.display()));
    String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        // Same two-shapes-of-line format as in `nm_symbols`; the demangled
        // name is everything after the kind column (it can itself contain
        // whitespace, e.g. generic parameter lists), so join the rest back.
        let parts: Vec<&str> = line.split_whitespace().collect();
        let (kind, demangled): (&str, String) = match parts.as_slice() {
            [_address, kind, rest @ ..] if !rest.is_empty() => (kind, rest.join(" ")),
            [kind, rest @ ..] if !rest.is_empty() => (kind, rest.join(" ")),
            _ => return false,
        };
        matches!(kind, "T" | "W" | "t" | "w")
            && (!demangled.contains("::") || demangled.starts_with("__rustc::"))
    })
}

/// Extracts every object file belonging to `TEMPORAL_BRIDGE_CRATES` out of
/// their `.rlib`s in the shared `target/<profile>/deps` directory.
/// `sako-temporal-bridge` is a `[build-dependencies]` entry of `sako-v8`,
/// which guarantees Cargo fully compiles it (and its own dependencies)
/// before `sako-v8`'s build script runs; by the time `sako-cli`'s build
/// script runs (after `sako-v8` is fully built), they're still there in the
/// same shared `deps` directory.
///
/// A crate in that list can have more than one hashed `.rlib` present: with
/// `resolver = "3"` (set workspace-wide), Cargo unifies features
/// separately for the build-dependency graph and the normal one, and even
/// within the build-dependency graph two consumers can pull in the same
/// crate with different enabled features — so e.g. `icu_calendar` or
/// `zerotrie` can legitimately end up compiled more than once with
/// genuinely different crate metadata hashes baked into every mangled
/// symbol they export. For crates like that, every candidate is extracted:
/// since each compile's symbol names are unique (see `has_unstable_defined_symbol`),
/// and each caller of theirs was compiled against a specific one of those
/// hashes, only the matching candidate's symbols ever actually get
/// referenced — there's no real conflict, just more objects than strictly
/// necessary on the link line.
///
/// The exception is any crate that (also) exports fixed, unmangled names —
/// `sako-temporal-bridge`, `temporal_capi`, `diplomat_runtime` — where
/// including more than one candidate genuinely does produce duplicate
/// symbols. For those, exactly one candidate must be picked, the same way
/// a linker would: starting from whichever of them has only one candidate
/// to begin with (there's always at least one — `sako-temporal-bridge`
/// itself, reachable only through this one dependency edge), pick another
/// only once something already-chosen is shown (via `nm`) to reference a
/// symbol it defines, and repeat until nothing new resolves.
fn temporal_bridge_objects(out_dir: &Path) -> Vec<PathBuf> {
    // OUT_DIR is `target/<profile>/build/<crate>-<hash>/out`.
    let deps_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("OUT_DIR was not the usual target/<profile>/build/<crate>/out shape")
        .join("deps");

    let mut candidates: std::collections::HashMap<&str, Vec<PathBuf>> =
        std::collections::HashMap::new();
    let entries = fs::read_dir(&deps_dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", deps_dir.display()));
    for entry in entries {
        let entry = entry.expect("failed to read a deps directory entry");
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stem) = file_name
            .strip_prefix("lib")
            .and_then(|name| name.strip_suffix(".rlib"))
        else {
            continue;
        };
        let Some((crate_name, _hash)) = stem.rsplit_once('-') else {
            continue;
        };
        let Some(&crate_name) = TEMPORAL_BRIDGE_CRATES
            .iter()
            .find(|&&name| name == crate_name)
        else {
            continue;
        };
        candidates.entry(crate_name).or_default().push(path);
    }

    for &name in TEMPORAL_BRIDGE_CRATES {
        if !candidates.contains_key(name) {
            panic!(
                "'{name}' was not found in {}; is sako-temporal-bridge still \
                 a build-dependency of sako-v8?",
                deps_dir.display()
            );
        }
    }

    let needs_single_pick: std::collections::HashSet<&str> = candidates
        .iter()
        .filter(|(_, paths)| paths.iter().any(|path| has_unstable_defined_symbol(path)))
        .map(|(&name, _)| name)
        .collect();

    let mut chosen: std::collections::HashMap<&str, PathBuf> = std::collections::HashMap::new();
    let mut needed: std::collections::HashSet<String> = std::collections::HashSet::new();

    for &name in &needs_single_pick {
        if let [only] = candidates[name].as_slice() {
            let (_defined, undefined) = nm_symbols(only);
            needed.extend(undefined);
            chosen.insert(name, only.clone());
        }
    }
    loop {
        let mut progressed = false;
        for &name in &needs_single_pick {
            if chosen.contains_key(name) {
                continue;
            }
            if let Some(path) = candidates[name].iter().find(|path| {
                let (defined, _undefined) = nm_symbols(path);
                defined.iter().any(|symbol| needed.contains(symbol))
            }) {
                let (_defined, undefined) = nm_symbols(path);
                needed.extend(undefined);
                chosen.insert(name, path.clone());
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    // Anything left unresolved is never referenced (via a symbol we can see
    // in this pass) by anything else in the chain — e.g.
    // sako-temporal-bridge itself, which is the root nothing here points
    // *into* it, only out of it. Cargo doesn't clean up a crate's older
    // hashed artifacts when it recompiles under a new one, so among
    // otherwise-indistinguishable candidates the most recently modified is
    // the one this build actually just produced.
    for &name in &needs_single_pick {
        chosen.entry(name).or_insert_with(|| {
            candidates[name]
                .iter()
                .max_by_key(|path| {
                    fs::metadata(path)
                        .and_then(|metadata| metadata.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                })
                .expect("candidates is never empty")
                .clone()
        });
    }

    let mut to_extract: Vec<(&str, &Path)> = Vec::new();
    for (&name, paths) in &candidates {
        if needs_single_pick.contains(name) {
            to_extract.push((name, chosen[name].as_path()));
        } else {
            to_extract.extend(paths.iter().map(|path| (name, path.as_path())));
        }
    }

    let mut objects = Vec::new();
    for (crate_name, path) in to_extract {
        let hash = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.rsplit_once('-'))
            .map_or("unknown", |(_, hash)| hash);
        let dest_dir = out_dir
            .join("temporal_objs")
            .join(format!("{crate_name}-{hash}"));
        // Clear any objects extracted here by a previous build (e.g. from a
        // now-stale hashed rlib) so they can't linger and cause duplicate
        // symbols against what we're about to extract.
        let _ = fs::remove_dir_all(&dest_dir);
        fs::create_dir_all(&dest_dir)
            .unwrap_or_else(|error| panic!("failed to create {}: {error}", dest_dir.display()));
        let status = Command::new("llvm-ar")
            .arg("x")
            .arg(path)
            .current_dir(&dest_dir)
            .status()
            .unwrap_or_else(|error| panic!("failed to start llvm-ar: {error}"));
        if !status.success() {
            panic!("failed to extract objects from {}", path.display());
        }
        for object in fs::read_dir(&dest_dir)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", dest_dir.display()))
        {
            let object = object.expect("failed to read an extracted object entry");
            if object.path().extension().and_then(|ext| ext.to_str()) == Some("o") {
                objects.push(object.path());
            }
        }
        println!("cargo:rerun-if-changed={}", path.display());
    }
    objects
}
