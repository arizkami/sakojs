//! Sako's JavaScript runtime library.
//!
//! The sources live in `js/` as ordered fragments of one bootstrap function;
//! `build.rs` concatenates them into `bootstrap.js` (ASCII-only, ready to be
//! loaded as an external one-byte V8 string).

/// The generated bootstrap script.
pub const BOOTSTRAP: &str = include_str!(concat!(env!("OUT_DIR"), "/bootstrap.js"));

// The runtime loads BOOTSTRAP as a Latin-1 external string.
const _: () = assert!(BOOTSTRAP.is_ascii(), "build.rs must escape non-ASCII characters");
