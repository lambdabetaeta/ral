//! The host process's facts: working directory, user and home, XDG bases and
//! ral's dot-files.
//!
//! [`crate::boot`] boots a `Shell` *in* a host process; this only reports on
//! one.  Nothing here reads language state: a `within [env: …]` overlay never
//! applies, and code holding a shell threads its overlay through
//! [`EnvVars::home`](crate::types::EnvVars::home) instead.  This module imports
//! nothing from the crate, so the host's facts are read here and only here.
//!
//! A fact that is absent stays absent: no `.`, no `/`, no placeholder.
//! Substituting one would spend the caller's only chance to say something
//! true, and every caller has a different true thing to say.

mod basedir;

use std::path::PathBuf;

pub use basedir::XdgKind;
pub(crate) use basedir::resolve_xdg;

pub(crate) const HOME_VARS: [&str; 2] = ["HOME", "USERPROFILE"];
pub(crate) const USER_VARS: [&str; 2] = ["USER", "USERNAME"];

/// The first of `keys` that `read` binds to a non-empty value.
///
/// An empty binding counts as none: `HOME=` names no directory, and admitting
/// `Some("")` is what once let `~/x` expand to `/x`.  Overlay-holding code
/// passes a `read` that consults its overlay first, so each key checks the
/// overlay, then the host, before the next key is tried.
pub(crate) fn first_bound(keys: &[&str], read: impl Fn(&str) -> Option<String>) -> Option<String> {
    keys.iter().find_map(|k| read(k).filter(|v| !v.is_empty()))
}

fn host_var(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// Process working directory, for callers with no shell to ask; shells go
/// through `Shell::cwd`, which honours a `within` override or a prior `cd`.
#[allow(
    clippy::disallowed_methods,
    reason = "host-env: the process cwd, for a caller with no shell to ask"
)]
pub fn cwd() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

/// `HOME` (then `USERPROFILE`) from the host process env alone.
///
/// The clippy denylist bans this function so every call site is a written
/// decision, carrying an `#[allow]` whose reason says why the read is
/// host-level.
pub fn home() -> Option<String> {
    first_bound(&HOME_VARS, host_var)
}

/// `USER` (then `USERNAME`) from the host process env alone; same discipline
/// as [`home`].
pub fn user() -> Option<String> {
    first_bound(&USER_VARS, host_var)
}

/// ral's own config/data/state base: an absolute `XDG_*_HOME`, else the
/// launching user's home joined with the kind's default.  `None` when nothing
/// absolute resolves, a relative `HOME` included.
#[allow(
    clippy::disallowed_methods,
    reason = "ral's own config/data live where the tool is installed: a script's env overlay must not relocate them"
)]
pub fn xdg(kind: XdgKind) -> Option<PathBuf> {
    resolve_xdg(kind, home().as_deref()).filter(|p| p.is_absolute())
}

/// `~/<name>` (e.g. `home_dot(".ralrc")`), the single-file convention rc
/// loaders probe as a fallback to the XDG form.
#[allow(
    clippy::disallowed_methods,
    reason = "the dot-file convention names the launching user's real home: a script's env overlay must not relocate it"
)]
pub fn home_dot(name: &str) -> Option<PathBuf> {
    Some(PathBuf::from(home()?).join(name)).filter(|p| p.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_bound_skips_empty_and_keeps_key_order() {
        let read = |k: &str| match k {
            "A" => Some(String::new()),
            "B" => Some("b".into()),
            "C" => Some("c".into()),
            _ => None,
        };
        assert_eq!(first_bound(&["A", "B", "C"], read), Some("b".into()));
        assert_eq!(first_bound(&["X", "A"], read), None);
    }
}
