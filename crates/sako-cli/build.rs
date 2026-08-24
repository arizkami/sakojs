// SPDX-License-Identifier: BSD-3-Clause

use std::env;

// Windows resolves every static import before `main` runs. Sako reaches
// dbghelp, winmm, ws2_32, and psapi only from paths a short script never
// takes -- stack symbolization, timer resolution, sockets, and process
// diagnostics -- so importing them eagerly charges a script that uses none of
// them for four extra module loads. Delay loading keeps the entry points and
// moves each load to the first call.
const DELAY_LOADED_LIBRARIES: [&str; 4] =
    ["dbghelp.dll", "winmm.dll", "ws2_32.dll", "psapi.dll"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    for library in DELAY_LOADED_LIBRARIES {
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{library}");
    }
    println!("cargo:rustc-link-arg-bins=delayimp.lib");
}
