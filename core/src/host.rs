//! The host process's facts: its environment, working directory, user and
//! home, temp directory, XDG bases, ral's dot-files and, on macOS, the user's
//! cache directory.
//!
//! [`crate::shell::boot`] boots a `Shell` *in* a host process; this only reports on
//! one.  The free functions read no language state: a `within [env: …]` overlay
//! never applies, and code holding a shell threads its overlay through
//! [`EnvVars::home`](crate::environ::EnvVars::home) instead; ral's rules over
//! these facts are [`crate::environ`]'s.  This module takes
//! from the crate only the pure rules of [`crate::path`] and the site
//! vocabulary, so the host's facts are read here and only here: [`var`],
//! [`var_os`], [`vars_os`] and [`temp_dir`] are the one reader of the host
//! environment.
//!
//! A fact that is absent stays absent: no `.`, no `/`, no placeholder.
//! Substituting one would spend the caller's only chance to say something
//! true, and every caller has a different true thing to say.

#[cfg(unix)]
pub mod login_env;
mod site;
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
pub(crate) mod test_support;

use std::ffi::OsString;
use std::path::PathBuf;

use self::site::Silent;
use crate::path::{CwdPath, XdgKind, is_absolute, resolve_xdg_in};
use crate::site::site;

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

/// The host process's variable `key`; `None` where unset or not Unicode.
///
/// The one reader of the host environment: a `within [env: …]` overlay never
/// applies, so code holding a shell reads its `EnvVars` instead.
pub fn var(key: &str) -> Option<String> {
    site!(Silent::HostVar, std::env::var(key)).ok()
}

/// [`var`], as the OS holds it.
pub fn var_os(key: &str) -> Option<OsString> {
    site!(Silent::HostVarOs, std::env::var_os(key))
}

/// Every variable of the host process, as the OS holds them.
pub fn vars_os() -> Vec<(OsString, OsString)> {
    site!(Silent::HostVars, std::env::vars_os()).collect()
}

/// The host's temp directory: `TMPDIR`, `TMP` or `TEMP`, else the OS's
/// default.
pub fn temp_dir() -> PathBuf {
    site!(Silent::HostTemp, std::env::temp_dir())
}

/// The account's home as the user database holds it, for a process whose
/// environment binds none: `std::env::home_dir`'s rule, `getpwuid_r` on Unix and
/// the profile folder on Windows.  `None` where the database has none, or none
/// that is Unicode.
fn account_home() -> Option<String> {
    site!(Silent::AccountHome, std::env::home_dir())?
        .into_os_string()
        .into_string()
        .ok()
}

/// Process working directory, for callers with no shell to ask and for the
/// shell a front end boots.
///
/// Shells go through `Shell::cwd`, which honours a `within` override or a
/// prior `cd`.  `None` when the OS has none to give, a removed directory say,
/// or gives one no relative path can be read against ([`CwdPath`]).
pub fn cwd() -> Option<CwdPath> {
    CwdPath::new(site!(Silent::HostCwd, std::env::current_dir()).ok()?).ok()
}

/// `HOME` (then `USERPROFILE`) from the host process env alone.
///
/// The clippy denylist bans this function so every call site is a written
/// decision: a typed `site!` whose variant says why the read is host-level.
pub fn home() -> Option<String> {
    first_bound(&HOME_VARS, var)
}

/// The home of ral's own layout as text: [`home`]'s read, else the
/// account's, so a program's files resolve where no `HOME` is bound.  A `~`
/// is [`home`]'s.
fn layout_home_var() -> Option<String> {
    first_bound(&HOME_VARS, var).or_else(account_home)
}

/// `USER` (then `USERNAME`) from the host process env alone; same discipline
/// as [`home`].
pub fn user() -> Option<String> {
    first_bound(&USER_VARS, var)
}

/// ral's own config/data/state base: an absolute `XDG_*_HOME`, else the
/// launching user's home joined with the kind's default.
///
/// Where no `HOME` is bound, the account's home from the user database stands
/// in for it, so a program's layout resolves under `env -i`, cron and systemd
/// units; a `~` in a script is [`home`]'s and never reads it.  `None` when
/// nothing absolute resolves, a relative `HOME` included.
pub fn xdg(kind: XdgKind) -> Option<PathBuf> {
    let home = layout_home_var();
    resolve_xdg_in(var, kind, home.as_deref()).filter(|p| is_absolute(p))
}

/// The home of ral's own layout ([`layout_home_var`]), where absolute.
pub fn layout_home() -> Option<PathBuf> {
    layout_home_var()
        .map(PathBuf::from)
        .filter(|p| is_absolute(p))
}

/// `~/<name>` (e.g. `home_dot(".ralrc")`) in [`layout_home`], the single-file
/// convention rc loaders probe as a fallback to the XDG form.
pub fn home_dot(name: &str) -> Option<PathBuf> {
    layout_home().map(|home| home.join(name))
}

/// This user's cache directory on macOS, `confstr(_CS_DARWIN_USER_CACHE_DIR)`,
/// its trailing `/` dropped; `None` where it names none, or none that is
/// Unicode.
#[cfg(target_os = "macos")]
pub(crate) fn user_cache_dir() -> Option<PathBuf> {
    let mut buf = [0u8; libc::PATH_MAX as usize];
    // SAFETY: `buf` is writable for the length passed.
    let n = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_CACHE_DIR,
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    let dir = (n > 1 && n <= buf.len()).then(|| &buf[..n - 1])?;
    Some(PathBuf::from(
        std::str::from_utf8(dir).ok()?.trim_end_matches('/'),
    ))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::host::test_support::with_vars_cleared;

    #[test]
    fn the_user_database_stands_in_for_an_unbound_home_in_the_layout_alone() {
        let unbound = ["HOME", "USERPROFILE", "XDG_CONFIG_HOME", "XDG_STATE_HOME"];
        with_vars_cleared(&unbound, || {
            let account = std::env::home_dir()
                .and_then(|home| home.into_os_string().into_string().ok())
                .filter(|home| is_absolute(home));
            assert_eq!(home(), None, "a `~` never reads the account");
            assert_eq!(
                home_dot(".x"),
                account.as_ref().map(|home| PathBuf::from(home).join(".x"))
            );
            assert_eq!(xdg(XdgKind::State).is_some(), account.is_some());
        });
    }

    #[test]
    fn a_bound_home_is_never_replaced_by_the_account() {
        with_vars_cleared(&["HOME", "USERPROFILE", "XDG_STATE_HOME"], || {
            // SAFETY: `with_vars_cleared` holds the environment guard, and
            // restores `HOME` when it returns.
            unsafe { std::env::set_var("HOME", "relative/home") };
            assert_eq!(
                xdg(XdgKind::State),
                None,
                "a relative HOME is bound, and refused"
            );
        });
    }

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
