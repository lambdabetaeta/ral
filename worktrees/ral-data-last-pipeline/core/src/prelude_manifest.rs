//! Build-generated list of names exported by `prelude.ral`.
//!
//! The `build.rs` script scans `src/prelude.ral` for top-level `let`
//! bindings and emits a `PRELUDE_EXPORTS` constant array into
//! `$OUT_DIR/prelude_manifest.rs`.  This module simply includes that
//! generated file so the rest of the crate can consult the manifest
//! at compile time (e.g. for head classification).

include!(concat!(env!("OUT_DIR"), "/prelude_manifest.rs"));
