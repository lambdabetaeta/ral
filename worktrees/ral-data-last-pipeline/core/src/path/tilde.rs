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
//! sigil expander in `path::sigil` (formerly `path::expand`) and
//! the `cd` builtin both go through this function so the rule is
//! one-and-the-same.

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

/// Look up `username`'s home directory via `getpwnam(3)`.  Falls
/// back to `/home/<name>` when the lookup fails or the username
/// contains a NUL byte.
#[cfg(unix)]
pub fn get_user_home(username: &str) -> String {
    use std::ffi::CString;
    let Ok(c_name) = CString::new(username) else {
        return format!("/home/{username}");
    };
    unsafe {
        let pw = libc::getpwnam(c_name.as_ptr());
        if pw.is_null() {
            return format!("/home/{username}");
        }
        std::ffi::CStr::from_ptr((*pw).pw_dir)
            .to_string_lossy()
            .into_owned()
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
