// SPDX-License-Identifier: BSD-3-Clause

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

include!("../sako-v8/build/linux_link.rs");

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        // Windows links sako-v8's static libraries through the normal
        // `cargo:rustc-link-lib`/`cc` crate mechanism, which (unlike the raw
        // linker arguments the Linux path needs — see below) does
        // propagate from sako-v8's own build script; nothing extra is
        // needed here.
        return;
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir
        .ancestors()
        .nth(2)
        .expect("sako-cli must be two levels below the workspace root")
        .to_path_buf();
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    let v8_root = env::var_os("SAKO_V8_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join(".deps").join("v8"));
    let v8_lib_dir = v8_root.join("lib");

    let libcxx = libcxx_root(&workspace_root);
    validate_libcxx(&libcxx);
    let libcxx_lib = libcxx.join("lib");

    // See crates/sako-v8/build.rs's `build_linux` for what these do and why
    // this crate — not sako-v8 — has to be the one emitting the actual
    // `cargo:rustc-link-arg`/`-lib` directives for them: raw
    // `cargo:rustc-link-arg` only applies to targets built within the
    // package that emits it, and doesn't propagate to a downstream binary
    // like this one that merely depends on sako-v8. See docs/v8-linkage.md.
    let force_string_o = force_libcxx_string_object(&libcxx_lib, &out_dir);
    // Unlike the standalone `bootstrap_cache` C++ tool sako-v8's build
    // script links (which needs `sysroot_objects()` because it isn't a
    // normal Rust binary), `sako-cli` *is* one — rustc already links
    // std/core/alloc/panic_abort/compiler_builtins into it automatically,
    // so pulling those in again here would just collide with them.
    let temporal_objects = temporal_bridge_objects(&out_dir);

    // sako-v8's build script compiled `bridge.cc` into a static library
    // sitting in its own `OUT_DIR`; `links = "sako_v8_bridge"` on that
    // crate is what makes Cargo expose that directory to us here via
    // `DEP_SAKO_V8_BRIDGE_ROOT` (the standard convention for a
    // `links`-owning build script to publish its `OUT_DIR` to dependents).
    let sako_v8_bridge_out_dir = env::var_os("DEP_SAKO_V8_BRIDGE_ROOT").unwrap_or_else(|| {
        panic!(
            "DEP_SAKO_V8_BRIDGE_ROOT was not set; is sako-v8's build script \
             still emitting `cargo:root=...`?"
        )
    });

    println!("cargo:rustc-link-arg=-fuse-ld=lld");
    // Some `temporal_objects` crates (e.g. `zerovec`, `icu_calendar`) are
    // *also* ordinary dependencies elsewhere in sako-cli's real dependency
    // graph (`idna`/`icu_normalizer` and friends use overlapping ICU
    // infrastructure). When Cargo's feature resolution happens to unify
    // that normal-dependency compile with the exact hash our matching in
    // `temporal_bridge_objects` picked, rustc's own automatic linking
    // already includes it — and our copy of the identical object then
    // looks like a duplicate definition to the linker. It genuinely is one
    // (byte-identical machine code, not a real conflict), so it's safe to
    // just let the linker keep whichever copy it sees first.
    println!("cargo:rustc-link-arg=-Wl,--allow-multiple-definition");
    println!("cargo:rustc-link-arg=-Wl,--start-group");
    println!(
        "cargo:rustc-link-arg=-L{}",
        PathBuf::from(&sako_v8_bridge_out_dir).display()
    );
    println!("cargo:rustc-link-arg=-lsako_v8_bridge");
    println!("cargo:rustc-link-arg=-L{}", v8_lib_dir.display());
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
    println!("cargo:rerun-if-env-changed=SAKO_V8_ROOT");
    println!("cargo:rerun-if-env-changed=SAKO_LIBCXX_ROOT");
    println!(
        "cargo:rerun-if-changed={}",
        v8_lib_dir.join("libv8_monolith.a").display()
    );
}
