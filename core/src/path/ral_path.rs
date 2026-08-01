//! `RAL_PATH` search: the only reader of the variable.
//!
//! `use` (in `builtins::modules`) calls [`find_file`] when script-relative
//! resolution fails; the plugin loader (`repl::plugin::load`, crate `ral`)
//! folds [`entries`] into a longer candidate chain of its own.

use std::path::PathBuf;

use super::Resolver;
use crate::types::EnvVars;

/// `RAL_PATH` directories in declaration order; empty when unset.
///
/// Reads `env_overrides` before the host env — the overlay-on-process rule
/// `within [env: …]` obeys.  `RAL_PATH` is a PATH-style search list, not a
/// single basedir.
pub fn entries(env_overrides: &EnvVars) -> Vec<PathBuf> {
    let raw = env_overrides.get_or_host("RAL_PATH").unwrap_or_default();
    std::env::split_paths(&raw).collect()
}

/// First `$dir/$name` that is a regular file, canonicalised, scanning
/// [`entries`] in order; `None` when nothing matches, an unset `RAL_PATH`
/// included.
///
/// A directory of that name is not a module, so it does not end the walk —
/// otherwise it would shadow a real file further down the list.
pub fn find_file(name: &str, env_overrides: &EnvVars) -> Option<PathBuf> {
    entries(env_overrides)
        .into_iter()
        .map(|dir| dir.join(name))
        .find_map(|cand| {
            Resolver::shell_less()
                .resolve(&cand.to_string_lossy())
                .canonicalise_strict()
                .ok()
                .filter(|p| p.is_file())
        })
}

#[cfg(test)]
#[cfg(unix)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/env scaffolding"
)]
mod tests {
    use super::*;

    #[test]
    fn find_file_walks_past_a_directory_bearing_the_name() {
        let tmp = tempfile::tempdir().unwrap();
        let (shadow, real) = (tmp.path().join("shadow"), tmp.path().join("real"));
        std::fs::create_dir_all(shadow.join("m.ral")).unwrap();
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("m.ral"), b"").unwrap();
        let raw = std::env::join_paths([&shadow, &real]).unwrap();

        let mut env = EnvVars::new();
        env.insert("RAL_PATH".into(), raw.to_string_lossy().into_owned());
        let found = find_file("m.ral", &env);

        assert_eq!(found, std::fs::canonicalize(real.join("m.ral")).ok());
    }
}
