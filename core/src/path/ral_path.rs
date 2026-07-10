//! `RAL_PATH` search, single source of truth.
//!
//! Two callers need to walk `RAL_PATH`: `use` (in
//! `builtins::modules`) as a fallback when script-relative
//! resolution fails, and the plugin loader (in
//! `ral::repl::plugin::load`) as one stage of its three-stage
//! search.  Both used to call `std::env::var_os("RAL_PATH")` +
//! `std::env::split_paths` themselves; this module owns those calls.
//!
//! API shape:
//!
//!   * [`entries`] yields the `RAL_PATH` directories in order so
//!     callers with their own search-chain composition (the plugin
//!     loader) can fold us into a bigger iterator.
//!
//!   * [`find_file`] is the common case: "find the first
//!     `$dir/$name` that canonicalises strictly".  Returns `None`
//!     when nothing matches, matching the contract of
//!     [`super::ResolvedPath::canonicalise_strict`] for missing
//!     paths.

use std::path::PathBuf;

use super::Resolver;

/// `RAL_PATH` directories in declaration order.  Empty when the
/// variable is unset.  Uses the platform's path separator (`:` on
/// Unix, `;` on Windows) via [`std::env::split_paths`].
// RAL_PATH is a PATH-style search list, not a single basedir.
#[allow(clippy::disallowed_methods)]
pub fn entries() -> Vec<PathBuf> {
    let raw = std::env::var_os("RAL_PATH").unwrap_or_default();
    std::env::split_paths(&raw).collect()
}

/// First `$dir/$name` that resolves strictly, scanning [`entries`] in
/// order.
///
/// Canonicalises through
/// [`ResolvedPath::canonicalise_strict`](super::ResolvedPath::canonicalise_strict);
/// `None` when nothing in `RAL_PATH` names an existing file (including
/// the case where the variable is unset).
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
