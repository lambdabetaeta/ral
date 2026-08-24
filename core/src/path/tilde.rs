//! Tilde paths: the `~`/`~user`/`~/sub` shape the lexer hands to the parser,
//! AST, IR and typechecker, and the one expansion of it.
//!
//! `path::sigil`, `cd`, command identity and REPL completion all resolve
//! through [`expand_tilde_path`], so the rule is one-and-the-same.
//!
//! Neither shape is always answerable, and both unanswerable cases are shaped
//! alike: a *named* user has no answer off Unix (no `getpwnam(3)` analogue),
//! and bare `~` has none where nothing binds `$HOME`.  So [`home`] and
//! [`get_user_home`] return `Option`, [`expand_tilde_path`] fails with the
//! [`Unexpandable`] cause, and each caller picks its own honest answer: the
//! policy freeze, `cd` and interpolation error; command resolution falls back
//! to the literal spelling; completion offers no candidates.
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

/// Why a tilde has no expansion.  The two causes take different fixes, so the
/// failure carries which one rather than leaving each caller to re-derive it
/// from the arguments it passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unexpandable {
    /// Nothing binds `$HOME`, so bare `~` names no directory.
    HomeUnknown,
    /// A `~user` this platform cannot look up.
    ForeignUser,
}

impl Unexpandable {
    /// The cause as a clause, the advice left to the caller — what to suggest
    /// depends on whether a shell user typed the tilde or a policy declared it.
    pub fn why(self) -> &'static str {
        match self {
            Self::HomeUnknown => "HOME is unset, so `~` names no directory",
            Self::ForeignUser => {
                "this platform cannot resolve another user's home directory \
                 (no getpwnam(3) equivalent)"
            }
        }
    }
}

/// Expand a tilde shape against `home`, the current user's home — `None` when
/// nothing binds it; a named `user` routes through [`get_user_home`] instead,
/// and so is unresolvable off Unix.
///
/// # Errors
/// [`Unexpandable::HomeUnknown`] for a bare `~` with no `home`,
/// [`Unexpandable::ForeignUser`] for a `~user` this platform cannot look up.
pub fn expand_tilde_path(
    user: Option<&str>,
    suffix: Option<&str>,
    home: Option<&str>,
) -> Result<String, Unexpandable> {
    let base = match user {
        None => home.ok_or(Unexpandable::HomeUnknown)?.to_string(),
        Some(user) => get_user_home(user).ok_or(Unexpandable::ForeignUser)?,
    };
    Ok(match suffix {
        None => base,
        Some(suffix) => format!("{base}{suffix}"),
    })
}

/// Fold a leading `home` prefix to `~` for display, inverting
/// [`expand_tilde_path`]'s `~` case.
///
/// The match is on component boundaries, so home `/home/al` leaves
/// `/home/alex` alone where a `starts_with` on the raw string would clip it.
pub fn abbreviate_home(path: &std::path::Path, home: Option<&str>) -> String {
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
fn abbreviate_home_for(path: &str, home: Option<&str>, windows: bool) -> String {
    let shown = match home {
        None => path.to_string(),
        Some(home) if windows => windows_strip_home(path, home),
        Some(home) => match std::path::Path::new(path).strip_prefix(home) {
            Ok(rest) => {
                if rest.as_os_str().is_empty() {
                    return "~".to_string();
                }
                format!("~/{}", rest.display())
            }
            Err(_) => path.to_string(),
        },
    };
    if windows {
        shown.replace('\\', "/")
    } else {
        shown
    }
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
    if !super::lex::starts_with_identity(path, home, true) {
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
/// before the host env; `None` when nothing binds one.
///
/// An empty binding counts as none: `HOME=` names no directory, and admitting
/// `Some("")` here is what once let `~/x` expand to `/x` — a syntactically
/// ordinary path meaning something nobody asked for.  Every downstream `~`,
/// `xdg:` and prompt fold therefore takes an `Option` and picks its own honest
/// answer, exactly as [`get_user_home`]'s callers do.
pub fn home(env_overrides: &crate::types::EnvVars) -> Option<String> {
    bound(env_overrides, "HOME").or_else(|| bound(env_overrides, "USERPROFILE"))
}

/// `$USER`, then `$USERNAME` (Windows), overrides before the host env; `None`
/// when nothing binds one.  Same discipline as [`home`] — the prompt and the
/// audit trail each name their own placeholder.
pub fn user_name(env_overrides: &crate::types::EnvVars) -> Option<String> {
    bound(env_overrides, "USER").or_else(|| bound(env_overrides, "USERNAME"))
}

/// One env var, overrides before the host env, an empty value read as unset.
fn bound(env_overrides: &crate::types::EnvVars, key: &str) -> Option<String> {
    env_overrides.get_or_host(key).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_tilde_expands_to_home_on_every_platform() {
        assert_eq!(
            expand_tilde_path(None, None, Some("/h")),
            Ok("/h".to_string())
        );
    }

    #[test]
    fn bare_tilde_with_suffix_expands_on_every_platform() {
        assert_eq!(
            expand_tilde_path(None, Some("/sub"), Some("/h")),
            Ok("/h/sub".to_string())
        );
    }

    /// The two unanswerable shapes, kept apart: an unset `$HOME` and a
    /// `~user` this platform cannot look up fail differently, so a caller can
    /// say which happened instead of guessing from what it passed in.
    #[test]
    fn bare_tilde_without_home_is_unexpandable_not_rooted_at_slash() {
        assert_eq!(
            expand_tilde_path(None, Some("/.gitconfig"), None),
            Err(Unexpandable::HomeUnknown)
        );
        assert_eq!(
            expand_tilde_path(None, None, None),
            Err(Unexpandable::HomeUnknown)
        );
    }

    /// An unknown home folds nothing — the prompt shows the path whole rather
    /// than an accidental `~`-relative reading of it.
    #[test]
    fn abbreviation_without_home_leaves_the_path_alone() {
        assert_eq!(abbreviate_home_for("/a/b", None, false), "/a/b");
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
        let foreign = Err(Unexpandable::ForeignUser);
        assert_eq!(expand_tilde_path(Some("bob"), None, Some("/h")), foreign);
        assert_eq!(
            expand_tilde_path(Some("bob"), Some("/sub"), Some("/h")),
            foreign
        );
    }

    // No `cfg(windows)` on the fold tests below: `windows` is a parameter,
    // and the fixtures are shaped so both hosts' `Path` parses agree (a
    // drive-letter *home* would strip only under a Windows-parsed `Path`,
    // so none appears here).

    #[test]
    fn abbreviation_renders_forward_slashes_on_windows() {
        assert_eq!(
            abbreviate_home_for(r"/h/projects\ral", Some("/h"), true),
            "~/projects/ral"
        );
    }

    #[test]
    fn abbreviation_folds_separators_in_the_unabbreviated_fallback_on_windows() {
        assert_eq!(
            abbreviate_home_for(r"D:\work\thing", Some(r"C:\Users\al"), true),
            "D:/work/thing"
        );
    }

    /// Off Windows `\` is an ordinary filename byte, so display folding must
    /// leave it alone.
    #[test]
    fn abbreviation_keeps_backslash_bytes_off_windows() {
        assert_eq!(
            abbreviate_home_for(r"/h/we\ird", Some("/h"), false),
            r"~/we\ird"
        );
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
            abbreviate_home_for("/h/users/MyProject", Some("/H/Users"), true),
            "~/MyProject"
        );
    }

    /// Same identity rule as `lex::starts_with_identity`'s own tests: a
    /// verbatim `\\?\` prefix and a case difference both fall away, and the
    /// tail keeps its own case regardless.
    #[test]
    fn abbreviation_strips_verbatim_prefix_and_case_on_windows() {
        assert_eq!(
            abbreviate_home_for(r"\\?\C:\Users\Al\Work", Some(r"c:\users\al"), true),
            "~/Work"
        );
    }

    /// A `Path::strip_prefix`-only rule would leave this printed in full: the
    /// case mismatch defeats a byte-exact strip even though the two spellings
    /// name the same directory under Windows identity.
    #[test]
    fn abbreviation_off_windows_stays_case_sensitive() {
        assert_eq!(
            abbreviate_home_for("/h/users/x", Some("/H/Users"), false),
            "/h/users/x"
        );
    }
}
