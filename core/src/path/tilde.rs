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
}

/// Look up `username`'s home directory via the reentrant
/// `getpwnam_r(3)`, which writes into a caller-owned buffer rather
/// than the shared static `passwd` that `getpwnam(3)` returns.
///
/// Falls back to `/home/<name>` when the lookup fails or the
/// username contains a NUL byte.
#[cfg(unix)]
pub fn get_user_home(username: &str) -> String {
    use std::ffi::CString;
    let fallback = || format!("/home/{username}");
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
        return unsafe {
            std::ffi::CStr::from_ptr((*result).pw_dir)
                .to_string_lossy()
                .into_owned()
        };
    }
}

#[cfg(not(unix))]
pub fn get_user_home(username: &str) -> String {
    format!("/home/{username}")
}

/// Expand a tilde shape against a home directory.
///
/// `home` is the current user's home (used for `~` and `~/...`);
/// `~user` / `~user/...` resolves through [`get_user_home`].  No
/// filesystem access — pure once `home` and `user` are fixed.
pub fn expand_tilde_path(user: Option<&str>, suffix: Option<&str>, home: &str) -> String {
    let base = match user {
        None => home.to_string(),
        Some(user) => get_user_home(user),
    };
    match suffix {
        None => base,
        Some(suffix) => format!("{base}{suffix}"),
    }
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
