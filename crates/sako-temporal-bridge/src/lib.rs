// SPDX-License-Identifier: BSD-3-Clause

//! Keeps `temporal_capi`'s `extern "C"` surface compiled into this crate's
//! object files so `sako-v8`'s build script can pull the definitions
//! V8's `libv8_monolith.a` needs out of them at link time. Nothing in this
//! crate is meant to be called from Rust; see `build.rs` for why the
//! keepalive list exists.

#[global_allocator]
static GLOBAL: std::alloc::System = std::alloc::System;

// rustc normally synthesizes a definition for this marker on the fly as
// part of its own final-link step for a "real" bin/staticlib/cdylib
// artifact (it guards against linking against an incompatible alloc-shim
// ABI version) — nothing in any precompiled `.rlib`, including the
// sysroot's, actually defines it. sako-v8's build script never runs
// rustc's own final link (see docs/v8-linkage.md), so nothing else would
// ever provide this; the actual value is never read, only its address
// matters for the check to link.
//
// It's a `#[rustc_std_internal_symbol]` item (an unstable, compiler-only
// attribute), which mangles under a fixed synthetic `___rustc` pseudo-crate
// rather than this crate's own identity — the same reason the allocator
// shim below (`#[global_allocator]`-generated) does. `#[rustc_attrs]` isn't
// available on stable, so the exact mangled name is pinned directly via
// `export_name` instead; it was read off with `nm` on the object that
// references it and will need updating if a future rustc changes it.
//
// `cfg(not(test))` because `cargo test` compiles this crate into a real,
// complete "bin"-shaped test harness binary — unlike sako-v8/sako-cli's
// manual link, THAT one *does* go through rustc's own normal final link,
// which synthesizes its own copy unconditionally regardless of our custom
// `#[global_allocator]`, colliding with ours.
#[cfg(not(test))]
#[unsafe(export_name = "_RNvCs9hJ03s5DiqP_7___rustc35___rust_no_alloc_shim_is_unstable_v2")]
pub static __RUST_NO_ALLOC_SHIM_IS_UNSTABLE_V2: u8 = 0;

include!(concat!(env!("OUT_DIR"), "/keepalive.rs"));
