//! Free-standing utilities with no evaluator dependency.
//!
//! Small helpers used across the crate that do not depend on the
//! evaluator, type checker, or any runtime state.

use crate::types::Value;
use serde::{Deserialize, Serialize};

/// Structured tilde path syntax: `~`, `~user`, `~/path`, or `~user/path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TildePath {
    pub user: Option<String>,
    pub suffix: Option<String>,
}

impl TildePath {
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

/// Parse a bare word into the most specific [`Value`] type.
///
/// Tries, in order: booleans (`true`/`false`), `unit`, integers,
/// floats (must contain `.`), and falls back to a string.
pub fn parse_literal(s: &str) -> Value {
    match s {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "unit" => Value::Unit,
        _ if s.parse::<i64>().is_ok() => Value::Int(s.parse().unwrap()),
        _ if s.contains('.') && s.parse::<f64>().is_ok() => Value::Float(s.parse().unwrap()),
        _ => Value::String(s.to_string()),
    }
}

/// Return the home directory for `username` via `getpwnam(3)`.
///
/// Falls back to `/home/<name>` when the lookup fails or the username
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

/// Expand a tilde path form to a concrete string.
///
/// `home` supplies the current user's home directory for bare `~` and `~/...`.
/// For `~user` / `~user/...`, the username is resolved via `get_user_home`.
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
