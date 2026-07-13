//! Tilde paths: parse and expand.
//!
//! `TildePath` is the syntactic shape produced by the lexer for
//! tilde-headed words (`~`, `~user`, `~/sub`, `~user/sub`).  Lives
//! here rather than in `util.rs` because expansion belongs to the
//! path-resolution pipeline; the lexer/parser/AST/IR/typecheck
//! layers all import it from here.
//!
//! `expand_tilde_path` is the only place that maps a tilde shape
//! to a concrete home-relative path; `get_user_home` is the
//! `getpwnam(3)` wrapper used for `~user` resolution.  The xdg
//! sigil expander in `path::sigil` and the `cd` builtin both go
//! through this function so the rule is one-and-the-same.
//!
//! `~user` (a *named* user, as opposed to bare `~`/`~/...`, which
//! never call [`get_user_home`]) has no answer off Unix: there is no
//! `getpwnam(3)` analogue, and Windows user profile directories aren't
//! enumerable from a username without an NT account lookup this
//! codebase doesn't carry.  [`get_user_home`] returns `None` there
//! rather than fabricating `/home/<name>`, and [`expand_tilde_path`]
//! propagates that as `None` — every caller decides its own honest
//! fallback (see the call sites: a policy freeze errors, a `cd`
//! errors, an interpolated value errors, PATH/command resolution and
//! tab completion pass the literal spelling through unexpanded).
//!
//! Also home to the `$HOME` / `$USER` lookup helpers (`home`,
//! `home_from_env`, `home_from_env_or_dot`, `user_name`,
//! `user_name_from_env`), which pin which env var each resolution reads
//! and what fallback it uses; `path.rs` re-exports them.

use serde::{Deserialize, Serialize};

/// Structured tilde path syntax: `~`, `~user`, `~/path`, or
/// `~user/path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TildePath {
    pub user: Option<String>,
    pub suffix: Option<String>,
}

impl TildePath {
    /// Recognise the shape; returns `None` when the input does
    /// not begin with `~`.
    pub fn parse(input: &str) -> Option<Self> {
        let rest = input.strip_prefix('~')?;
        match rest.split_once('/') {
            None => Some(Self {
                user: Some(rest.to_string()).filter(|s| !s.is_empty()),
                suffix: None,
            }),
            Some((user, suffix)) => Some(Self {
                user: Some(user.to_string()).filter(|s| !s.is_empty()),
                suffix: Some(format!("/{suffix}")),
            }),
        }
    }

    /// Reconstruct the literal `~user/suffix` spelling this value
    /// parsed from.  Used as the honest fallback at call sites that
    /// cannot fail (PATH/command resolution) when [`expand_tilde_path`]
    /// returns `None`: passing the un-expanded spelling through means
    /// resolution fails downstream as an ordinary missing-path/missing-
    /// command error, rather than silently matching a fabricated path.
    pub fn to_literal(&self) -> String {
        let user = self.user.as_deref().unwrap_or_default();
        let suffix = self.suffix.as_deref().unwrap_or_default();
        format!("~{user}{suffix}")
    }
}

/// Look up `username`'s home directory via the reentrant
/// `getpwnam_r(3)`, which writes into a caller-owned buffer rather
/// than the shared static `passwd` that `getpwnam(3)` returns.
///
/// Falls back to `/home/<name>` when the lookup fails or the
/// username contains a NUL byte.
#[cfg(unix)]
pub fn get_user_home(username: &str) -> Option<String> {
    use std::ffi::CString;
    let fallback = || Some(format!("/home/{username}"));
    let Ok(c_name) = CString::new(username) else {
        return fallback();
    };
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // `_SC_GETPW_R_SIZE_MAX` is only a hint; grow on `ERANGE`.
    let hint = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "guarded hint > 0; a positive getpw buffer-size hint fits usize"
    )]
    let mut len = if hint > 0 { hint as usize } else { 1024 };
    loop {
        let mut buf = vec![0u8; len];
        let rc = unsafe {
            libc::getpwnam_r(
                c_name.as_ptr(),
                &raw mut pwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &raw mut result,
            )
        };
        if rc == libc::ERANGE {
            len *= 2;
            continue;
        }
        if rc != 0 || result.is_null() {
            return fallback();
        }
        return Some(unsafe {
            std::ffi::CStr::from_ptr((*result).pw_dir)
                .to_string_lossy()
                .into_owned()
        });
    }
}

/// No `getpwnam(3)` analogue exists off Unix, and there is no honest
/// way to turn a bare username into a Windows profile directory
/// without an account lookup this codebase doesn't carry — so a named
/// user's home is unresolvable here.  Bare `~`/`~/...` never call this
/// (they resolve against the caller-supplied `home`, i.e. `%USERPROFILE%`
/// on Windows), so only `~user`/`~user/...` are affected.
#[cfg(not(unix))]
pub fn get_user_home(_username: &str) -> Option<String> {
    None
}

/// Expand a tilde shape against a home directory.
///
/// `home` is the current user's home (used for `~` and `~/...`, always
/// resolvable); `~user` / `~user/...` resolves through
/// [`get_user_home`], which is `None` off Unix — see the module note.
/// `None` propagates; every caller picks its own honest fallback. No
/// filesystem access — pure once `home` and `user` are fixed.
pub fn expand_tilde_path(user: Option<&str>, suffix: Option<&str>, home: &str) -> Option<String> {
    let base = match user {
        None => home.to_string(),
        Some(user) => get_user_home(user)?,
    };
    Some(match suffix {
        None => base,
        Some(suffix) => format!("{base}{suffix}"),
    })
}

/// Abbreviate `path` for display by folding a leading `home` prefix to
/// `~`. The inverse of [`expand_tilde_path`] for the `~` case.
///
/// `home` must match on a path-component boundary: home `/home/al`
/// abbreviates `/home/al` and `/home/al/src`, but leaves `/home/alex`
/// untouched (a `starts_with` on the raw string would wrongly clip it).
/// Returns the path's own string form when `home` is empty or is not a
/// component prefix.
pub fn abbreviate_home(path: &std::path::Path, home: &str) -> String {
    if home.is_empty() {
        return path.to_string_lossy().into_owned();
    }
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

// ── $HOME / $USER lookup ──────────────────────────────────────────────

/// Look up `HOME`, preferring the supplied dynamic env overrides:
/// `$HOME`, then `$USERPROFILE` (Windows).  Empty string when
/// nothing is set.
pub fn home(env_overrides: &crate::types::EnvVars) -> String {
    env_overrides
        .get_or_host("HOME")
        .or_else(|| env_overrides.get_or_host("USERPROFILE"))
        .unwrap_or_default()
}

/// Look up `HOME` from the process env only — for callers that
/// have no dynamic env at hand (REPL completion, policy loaders).
pub fn home_from_env() -> String {
    home(&crate::types::EnvVars::new())
}

/// `home_from_env`, falling back to `.` when unset, so callers that
/// join paths against home never panic on an empty base (REPL
/// completion, env seeding).
pub fn home_from_env_or_dot() -> String {
    let h = home_from_env();
    if h.is_empty() { ".".into() } else { h }
}

/// Look up the current user name, preferring the supplied dynamic
/// env overrides: `$USER`, then `$USERNAME` (Windows).  `"?"` when
/// nothing is set, matching the prompt/audit placeholder.
pub fn user_name(env_overrides: &crate::types::EnvVars) -> String {
    env_overrides
        .get_or_host("USER")
        .or_else(|| env_overrides.get_or_host("USERNAME"))
        .unwrap_or_else(|| "?".into())
}

/// Look up the user name from the process env only — for callers
/// with no dynamic env at hand (REPL prompt, host snapshot, shell
/// seeding).
pub fn user_name_from_env() -> String {
    user_name(&crate::types::EnvVars::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_tilde_expands_to_home_on_every_platform() {
        assert_eq!(expand_tilde_path(None, None, "/h"), Some("/h".to_string()));
    }

    #[test]
    fn bare_tilde_with_suffix_expands_on_every_platform() {
        assert_eq!(
            expand_tilde_path(None, Some("/sub"), "/h"),
            Some("/h/sub".to_string())
        );
    }

    /// Unix's `get_user_home` fabricates a conventional `/home/<name>`
    /// only as a last resort, when `getpwnam_r` itself cannot find the
    /// user — a pre-existing behaviour this task doesn't change. Uses a
    /// username vanishingly unlikely to exist so the test hits that
    /// fallback branch rather than a real account.
    #[cfg(unix)]
    #[test]
    fn unix_named_user_falls_back_when_lookup_misses() {
        let home = get_user_home("ral-tilde-test-no-such-user-8f3c1a");
        assert_eq!(home, Some("/home/ral-tilde-test-no-such-user-8f3c1a".to_string()));
    }

    /// The behaviour the plan requires: off Unix there is no
    /// `getpwnam(3)` analogue, so a *named* user's home is
    /// unresolvable — `None`, never a fabricated `/home/<name>`. This
    /// only runs where it is meaningful (Windows CI); on Unix hosts the
    /// sibling test above pins the real behaviour instead.
    #[cfg(not(unix))]
    #[test]
    fn non_unix_named_user_is_unresolvable_not_fabricated() {
        assert_eq!(get_user_home("bob"), None);
        assert_eq!(expand_tilde_path(Some("bob"), None, "/h"), None);
        assert_eq!(expand_tilde_path(Some("bob"), Some("/sub"), "/h"), None);
    }

    #[test]
    fn to_literal_reconstructs_the_parsed_spelling() {
        assert_eq!(TildePath::parse("~bob/sub").unwrap().to_literal(), "~bob/sub");
        assert_eq!(TildePath::parse("~bob").unwrap().to_literal(), "~bob");
        assert_eq!(TildePath::parse("~/sub").unwrap().to_literal(), "~/sub");
        assert_eq!(TildePath::parse("~").unwrap().to_literal(), "~");
    }
}
