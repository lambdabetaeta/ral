//! Tilde paths: the `~`/`~user`/`~/sub` shape the lexer hands to the parser,
//! AST, IR and typechecker, and the one expansion of it.
//!
//! `path::sigil`, `cd`, command identity and REPL completion all resolve
//! through [`expand_tilde_path`], so the rule is one-and-the-same.
//!
//! A *named* user has no answer off Unix (no `getpwnam(3)` analogue), so
//! [`get_user_home`] returns `None` rather than fabricate `/home/<name>` and
//! each caller picks its own honest fallback: the policy freeze, `cd` and
//! interpolation error; command resolution falls back to the literal spelling;
//! completion offers no candidates.  Bare `~`/`~/...` never come this way.
//!
//! Also the `$HOME`/`$USER` lookups, which pin the env var each one reads;
//! `path.rs` re-exports them.

use serde::{Deserialize, Serialize};

/// Structured tilde syntax: `~`, `~user`, `~/path`, or `~user/path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TildePath {
    pub user: Option<String>,
    pub suffix: Option<String>,
}

impl TildePath {
    /// Recognise the shape; `None` when the input does not begin with `~`.
    ///
    /// The public door: `windows` is read from `cfg!` here and nowhere below,
    /// so [`Self::parse_for`] carries the actual rule and can be pinned on
    /// every host in tests, matching `lex::starts_with_identity`'s split
    /// between a platform-reading door and a platform-taking parameter.
    pub fn parse(input: &str) -> Option<Self> {
        Self::parse_for(input, cfg!(windows))
    }

    /// [`Self::parse`]'s rule, `windows` a parameter rather than the `cfg!`
    /// read at the door.
    ///
    /// Off Windows only `/` separates the user part from the suffix, as
    /// before. Under Windows `\` counts too — `~\sub` typed at a Windows
    /// prompt is exactly the shape `~/sub` is elsewhere, and splitting only on
    /// `/` would leave it a literal `user = "\sub"` with no suffix, which then
    /// fails to expand at all.
    ///
    /// The suffix keeps whichever separator byte it was split on — `/sub` or
    /// `\sub` — rather than normalising to one spelling: [`Self::to_literal`]
    /// promises to reconstruct *the spelling this parsed from*, and callers
    /// that echo it back in an error message (a `~user` this platform cannot
    /// resolve) should echo what the user actually typed. Nothing downstream
    /// cares which separator survives — [`expand_tilde_path`] only
    /// concatenates, and the resolver it feeds accepts either.
    fn parse_for(input: &str, windows: bool) -> Option<Self> {
        let rest = input.strip_prefix('~')?;
        let split = if windows {
            rest.find(['/', '\\'])
        } else {
            rest.find('/')
        };
        match split {
            None => Some(Self {
                user: Some(rest.to_string()).filter(|s| !s.is_empty()),
                suffix: None,
            }),
            Some(idx) => Some(Self {
                user: Some(rest[..idx].to_string()).filter(|s| !s.is_empty()),
                suffix: Some(rest[idx..].to_string()),
            }),
        }
    }

    /// Reconstruct the spelling this parsed from — the fallback at call sites
    /// that cannot fail, where an unexpanded `~user` dies downstream as an
    /// ordinary missing-command error rather than matching a fabricated path.
    pub fn to_literal(&self) -> String {
        let user = self.user.as_deref().unwrap_or_default();
        let suffix = self.suffix.as_deref().unwrap_or_default();
        format!("~{user}{suffix}")
    }
}

/// `username`'s home via the reentrant `getpwnam_r(3)`, falling back to the
/// conventional `/home/<name>` when the lookup misses or the name contains a
/// NUL byte.
#[cfg(unix)]
pub fn get_user_home(username: &str) -> Option<String> {
    match nix::unistd::User::from_name(username) {
        Ok(Some(user)) => Some(user.dir.to_string_lossy().into_owned()),
        _ => Some(format!("/home/{username}")),
    }
}

/// A named user's home is unresolvable off Unix — there is no `getpwnam(3)`
/// analogue and no way to turn a bare username into a Windows profile
/// directory without an account lookup this codebase doesn't carry.
#[cfg(not(unix))]
#[allow(
    clippy::too_long_first_doc_paragraph,
    reason = "the summary is one sentence with no interior stop: its only seam is an em dash, so a paragraph break there would leave rustdoc's item list an unterminated clause and open the next paragraph with a dangling dash"
)]
pub fn get_user_home(_username: &str) -> Option<String> {
    None
}

/// Expand a tilde shape against `home`, the current user's home; a named
/// `user` routes through [`get_user_home`] instead, and so is `None` off Unix.
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

/// Fold a leading `home` prefix to `~` for display, inverting
/// [`expand_tilde_path`]'s `~` case.
///
/// The match is on component boundaries, so home `/home/al` leaves
/// `/home/alex` alone where a `starts_with` on the raw string would clip it.
pub fn abbreviate_home(path: &std::path::Path, home: &str) -> String {
    abbreviate_home_for(&path.to_string_lossy(), home, cfg!(windows))
}

/// [`abbreviate_home`] on strings, `windows` a parameter rather than a `cfg!`
/// read as in `lex::starts_with_identity`, so the fold below is pinned on
/// every host.
///
/// Under Windows the result is folded to `/` separators: the `~/` head
/// already commits the string to them, so a native-separator rest would print
/// mixed (`~/projects\ral`), and folding the fallback arm too keeps the two
/// shapes consistent.  Display only — never fed back into resolution or
/// matching, which accept either spelling anyway.  Off Windows `\` is an
/// ordinary filename byte, hence the gate.
#[allow(
    clippy::disallowed_methods,
    reason = "lexical Path::new for the component-boundary strip — no I/O behind it; this module is part of crate::path, where the path-construction rule lives"
)]
fn abbreviate_home_for(path: &str, home: &str, windows: bool) -> String {
    let shown = if windows {
        windows_strip_home(path, home)
    } else {
        match std::path::Path::new(path).strip_prefix(home) {
            Ok(rest) if !home.is_empty() => {
                if rest.as_os_str().is_empty() {
                    return "~".to_string();
                }
                format!("~/{}", rest.display())
            }
            _ => path.to_string(),
        }
    };
    if windows { shown.replace('\\', "/") } else { shown }
}

/// The Windows half of [`abbreviate_home_for`]'s strip: containment under
/// [`super::lex::starts_with_identity`]'s identity rather than
/// `Path::strip_prefix`, which is separator-insensitive but
/// case-*sensitive* — so `USERPROFILE`/`cwd` disagreeing on casing
/// (`C:\Users\al` vs `c:\users\al`) would otherwise leave the prompt showing
/// the whole path instead of folding it to `~`.
///
/// The identity check only decides *whether* home is a prefix; the displayed
/// tail is sliced from `path`'s own components, in `path`'s own casing, so
/// the user's typed spelling survives — only the fold to `~`, never a fold to
/// `home`'s case, happens here.
fn windows_strip_home(path: &str, home: &str) -> String {
    if home.is_empty() || !super::lex::starts_with_identity(path, home, true) {
        return path.to_string();
    }
    let home_depth = super::lex::windows_identity_components(home).len();
    // The same verbatim/UNC normalisation `windows_identity_components`
    // folds before it lower-cases and splits, mirrored here without the case
    // fold: the displayed tail must keep `path`'s own casing, not `home`'s.
    let stripped = super::lex::strip_verbatim_prefix(path);
    let stripped = stripped
        .strip_prefix("UNC\\")
        .or_else(|| stripped.strip_prefix("UNC/"))
        .map_or_else(|| stripped.to_string(), |rest| format!(r"\{rest}"));
    let tail: Vec<&str> = stripped
        .split(['/', '\\'])
        .filter(|c| !c.is_empty())
        .skip(home_depth)
        .collect();
    if tail.is_empty() {
        "~".to_string()
    } else {
        format!("~/{}", tail.join("/"))
    }
}

// ── $HOME / $USER lookup ──────────────────────────────────────────────

/// `$HOME`, then `$USERPROFILE` (Windows), each read from `env_overrides`
/// before the host env; empty string when nothing is set.
pub fn home(env_overrides: &crate::types::EnvVars) -> String {
    env_overrides
        .get_or_host("HOME")
        .or_else(|| env_overrides.get_or_host("USERPROFILE"))
        .unwrap_or_default()
}

/// `$USER`, then `$USERNAME` (Windows), overrides before the host env; `"?"`
/// when nothing is set, matching the prompt/audit placeholder.
pub fn user_name(env_overrides: &crate::types::EnvVars) -> String {
    env_overrides
        .get_or_host("USER")
        .or_else(|| env_overrides.get_or_host("USERNAME"))
        .unwrap_or_else(|| "?".into())
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

    /// The username is chosen to be vanishingly unlikely to exist, so the test
    /// reaches the `/home/<name>` fallback rather than a real account.
    #[cfg(unix)]
    #[test]
    fn unix_named_user_falls_back_when_lookup_misses() {
        let home = get_user_home("ral-tilde-test-no-such-user-8f3c1a");
        assert_eq!(
            home,
            Some("/home/ral-tilde-test-no-such-user-8f3c1a".to_string())
        );
    }

    /// Runs only where it is meaningful (Windows CI); the sibling test above
    /// pins the Unix half.
    #[cfg(not(unix))]
    #[test]
    fn non_unix_named_user_is_unresolvable_not_fabricated() {
        assert_eq!(get_user_home("bob"), None);
        assert_eq!(expand_tilde_path(Some("bob"), None, "/h"), None);
        assert_eq!(expand_tilde_path(Some("bob"), Some("/sub"), "/h"), None);
    }

    // No `cfg(windows)` on the fold tests below: `windows` is a parameter,
    // and the fixtures are shaped so both hosts' `Path` parses agree (a
    // drive-letter *home* would strip only under a Windows-parsed `Path`,
    // so none appears here).

    #[test]
    fn abbreviation_renders_forward_slashes_on_windows() {
        assert_eq!(
            abbreviate_home_for(r"/h/projects\ral", "/h", true),
            "~/projects/ral"
        );
    }

    #[test]
    fn abbreviation_folds_separators_in_the_unabbreviated_fallback_on_windows() {
        assert_eq!(
            abbreviate_home_for(r"D:\work\thing", r"C:\Users\al", true),
            "D:/work/thing"
        );
    }

    /// Off Windows `\` is an ordinary filename byte, so display folding must
    /// leave it alone.
    #[test]
    fn abbreviation_keeps_backslash_bytes_off_windows() {
        assert_eq!(abbreviate_home_for(r"/h/we\ird", "/h", false), r"~/we\ird");
    }

    #[test]
    fn to_literal_reconstructs_the_parsed_spelling() {
        assert_eq!(
            TildePath::parse("~bob/sub").unwrap().to_literal(),
            "~bob/sub"
        );
        assert_eq!(TildePath::parse("~bob").unwrap().to_literal(), "~bob");
        assert_eq!(TildePath::parse("~/sub").unwrap().to_literal(), "~/sub");
        assert_eq!(TildePath::parse("~").unwrap().to_literal(), "~");
    }

    // `parse_for` rather than `parse` below: `windows` pinned as a parameter,
    // so the Windows separator rule is exercised on every host.

    #[test]
    fn backslash_suffix_parses_as_tilde_with_suffix_on_windows() {
        assert_eq!(
            TildePath::parse_for(r"~\sub", true),
            Some(TildePath {
                user: None,
                suffix: Some(r"\sub".to_string()),
            })
        );
        assert_eq!(
            TildePath::parse_for(r"~bob\sub", true),
            Some(TildePath {
                user: Some("bob".to_string()),
                suffix: Some(r"\sub".to_string()),
            })
        );
    }

    /// Off Windows `\` is an ordinary filename byte, so the whole rest is the
    /// (unresolvable, but honestly reported) user part, not a suffix split.
    #[test]
    fn backslash_suffix_is_not_a_separator_off_windows() {
        assert_eq!(
            TildePath::parse_for(r"~\sub", false),
            Some(TildePath {
                user: Some(r"\sub".to_string()),
                suffix: None,
            })
        );
    }

    /// [`TildePath::to_literal`] promises the parsed spelling back, so a
    /// backslash suffix must survive the round trip unnormalised.
    #[test]
    fn backslash_suffix_round_trips_through_to_literal() {
        assert_eq!(
            TildePath::parse_for(r"~\sub", true).unwrap().to_literal(),
            r"~\sub"
        );
    }

    /// `USERPROFILE` and the path under test disagree on casing — exactly the
    /// drift `Path::strip_prefix` cannot see past, since it folds separators
    /// but not case.  The displayed tail keeps `path`'s own casing (`MyProject`,
    /// not `myproject`): only the prefix comparison is case-insensitive, not
    /// the fold that renders `MyProject` on top of it.
    #[test]
    fn abbreviation_is_case_insensitive_to_home_on_windows() {
        assert_eq!(
            abbreviate_home_for("/h/users/MyProject", "/H/Users", true),
            "~/MyProject"
        );
    }

    /// Same identity rule as `lex::starts_with_identity`'s own tests: a
    /// verbatim `\\?\` prefix and a case difference both fall away, and the
    /// tail keeps its own case regardless.
    #[test]
    fn abbreviation_strips_verbatim_prefix_and_case_on_windows() {
        assert_eq!(
            abbreviate_home_for(r"\\?\C:\Users\Al\Work", r"c:\users\al", true),
            "~/Work"
        );
    }

    /// A `Path::strip_prefix`-only rule would leave this printed in full: the
    /// case mismatch defeats a byte-exact strip even though the two spellings
    /// name the same directory under Windows identity.
    #[test]
    fn abbreviation_off_windows_stays_case_sensitive() {
        assert_eq!(
            abbreviate_home_for("/h/users/x", "/H/Users", false),
            "/h/users/x"
        );
    }
}
