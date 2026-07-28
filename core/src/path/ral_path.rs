//! `RAL_PATH` search: the only reader of the variable.
//!
//! `use` (in `builtins::modules`) calls [`find_file`] when script-relative
//! resolution fails; the plugin loader (`repl::plugin::load`, crate `ral`)
//! folds [`entries`] into a longer candidate chain of its own.

use std::path::PathBuf;

use super::Resolver;

/// `RAL_PATH` directories in declaration order; empty when unset.
// RAL_PATH is a PATH-style search list, not a single basedir.
#[allow(clippy::disallowed_methods)]
pub fn entries() -> Vec<PathBuf> {
    let raw = std::env::var_os("RAL_PATH").unwrap_or_default();
    std::env::split_paths(&raw).collect()
}

/// First existing `$dir/$name`, canonicalised, scanning [`entries`] in
/// order; `None` when nothing matches, an unset `RAL_PATH` included.
pub fn find_file(name: &str) -> Option<PathBuf> {
    entries()
        .into_iter()
        .map(|dir| dir.join(name))
        .find_map(|cand| {
            Resolver::shell_less()
                .resolve(&cand.to_string_lossy())
                .canonicalise_strict()
                .ok()
        })
}
