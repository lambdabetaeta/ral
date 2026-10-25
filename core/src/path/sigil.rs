//! Run-time expansion of the path-prefix sigils `~[/sub]` and `xdg:NAME[/sub]`
//! at the head of a path.  Anything else passes through unchanged.
//!
//! They expand only here, at run time (stage 1 of
//! [`crate::path::Resolver`]): a grant names its places with tags, which
//! `authority::grant::freeze` owns, and refuses these prefixes.
//! XDG uses the Linux defaults on every platform, macOS included
//! ([`crate::host`]).

use crate::host::EnvVars;
use crate::path::basedir::resolve_xdg_in;
use crate::path::lex::join_relative;
use crate::path::tilde::TildePath;
use crate::path::{PathRules, XdgKind};

/// Parse `xdg:NAME[/sub]` under `rules`, the name ending at a separator as
/// `~` does: `/`, or `\` under Windows.  The subpath is everything after
/// that one separator, as written, for [`join_relative`] to judge whole.
/// `None` for a non-`xdg:` input or an unknown name.
fn parse_xdg_token(input: &str, rules: PathRules) -> Option<(XdgKind, Option<&str>)> {
    let body = input.strip_prefix("xdg:")?;
    let (name, sub) = match body.split_once(|c| rules.is_separator(c)) {
        Some((n, s)) => (n, Some(s)),
        None => (body, None),
    };
    Some((name.parse().ok()?, sub))
}

/// A run-time sigil at the head of a path: what [`expand_path_prefix`]
/// would read there, and what a grant must therefore refuse to take
/// literally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SigilHead<'a> {
    /// `~` or `~/sub`.
    Tilde(TildePath),
    /// `xdg:NAME[/sub]`, `NAME` a known kind, `sub` as written past the
    /// separator that ends the name.
    Xdg(XdgKind, Option<&'a str>),
    /// `xdg:` and a name no kind answers to, which expansion passes through
    /// literally.
    UnknownXdg,
}

impl<'a> SigilHead<'a> {
    /// The sigil `input` opens with under the host's rules, if any.
    pub(crate) fn of(input: &'a str) -> Option<Self> {
        Self::of_for(input, PathRules::HOST)
    }

    /// [`Self::of`] under given rules: the separator that ends the name of an
    /// `xdg:` head, or follows a `~`, is one of `rules`' own.
    pub(crate) fn of_for(input: &'a str, rules: PathRules) -> Option<Self> {
        if let Some((kind, sub)) = parse_xdg_token(input, rules) {
            return Some(Self::Xdg(kind, sub));
        }
        if input.starts_with("xdg:") {
            return Some(Self::UnknownXdg);
        }
        TildePath::parse_for(input, rules).map(Self::Tilde)
    }
}

/// A relative `name` spelled so [`expand_path_prefix`] leaves it as it is:
/// `./~/x` for a directory named `~`.
pub(crate) fn literal_relative(name: &str) -> String {
    if matches!(
        SigilHead::of(name),
        Some(SigilHead::Tilde(_) | SigilHead::Xdg(..))
    ) {
        format!("./{name}")
    } else {
        name.to_owned()
    }
}

/// Expand a `~` or `xdg:` head, or return `input` unchanged.  The runtime half
/// of expansion: no filesystem access.
///
/// `env.home()` is both the tilde root and the fallback when an `XDG_*_HOME`
/// is unset; both read `env`, then the host.
///
/// The expansion is native, in the host's own spelling, for the resolver to
/// hand on to the OS: `~` as [`TildePath::expand`] expands it, and the `xdg:`
/// sub-path joined as [`join_relative`] joins it, its leading separators
/// dropped and `..` included, for the resolver to fold.
///
/// Infallible, because [`Resolver::resolve`](super::Resolver::resolve) is:
/// whatever cannot be answered passes through literally — an unknown `home`
/// leaves `~/x` as `~/x`, an expansion that is not UTF-8 is no string to
/// expand into, and an `xdg:` sub-path naming a volume of its own, which
/// would re-root the join, leaves the whole path as written: under Windows,
/// a drive component (`xdg:config/C:\x`) or a share or namespace head
/// opening the sub-path (`xdg:config/\\?\UNC\srv\sh`, `xdg:config///x`).
/// The unexpanded literal is then anchored against `cwd` (`<cwd>/~/x`) rather
/// than fabricating a root-anchored `/.gitconfig`.  A grant refuses the gap
/// instead, in `authority::grant::freeze::FreezeCtx::place`.
pub fn expand_path_prefix(input: &str, env: &EnvVars) -> String {
    let expanded = match SigilHead::of(input) {
        Some(SigilHead::Xdg(kind, sub)) => {
            resolve_xdg_in(|k| env.get_or_host(k), kind, env.home().as_deref())
                .and_then(|base| join_relative(base, sub.unwrap_or_default()).ok())
        }
        Some(SigilHead::Tilde(t)) => env.home().map(|home| t.expand(&home)),
        Some(SigilHead::UnknownXdg) | None => None,
    };
    expanded
        .and_then(|path| path.into_os_string().into_string().ok())
        .unwrap_or_else(|| input.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(dir: &str) -> EnvVars {
        std::iter::once(("HOME", dir)).collect()
    }

    #[test]
    fn parse_xdg_token_recognises_each_kind() {
        for (name, kind) in [
            ("config", XdgKind::Config),
            ("data", XdgKind::Data),
            ("cache", XdgKind::Cache),
            ("state", XdgKind::State),
            ("bin", XdgKind::Bin),
        ] {
            let token = format!("xdg:{name}");
            let (k, sub) = parse_xdg_token(&token, PathRules::HOST).expect("known kind");
            assert_eq!(k, kind);
            assert!(sub.is_none());
        }
    }

    #[test]
    fn parse_xdg_token_carries_subpath() {
        let (k, sub) = parse_xdg_token("xdg:config/agda/lib", PathRules::HOST).unwrap();
        assert_eq!(k, XdgKind::Config);
        assert_eq!(sub, Some("agda/lib"));
    }

    #[test]
    fn parse_xdg_token_rejects_unknown_name() {
        assert!(parse_xdg_token("xdg:cofnig", PathRules::HOST).is_none());
    }

    #[test]
    fn parse_xdg_token_rejects_non_xdg() {
        assert!(parse_xdg_token("/etc", PathRules::HOST).is_none());
    }

    /// The name ends at one separator under each rules' own separators, and
    /// the sub-path is the rest as written: under Posix a backslash is a
    /// filename byte, so `xdg:config\x` names no kind.
    #[test]
    fn parse_xdg_token_reads_the_separators_of_its_rules() {
        for (input, posix, windows) in [
            ("xdg:config//etc", Some("/etc"), Some("/etc")),
            (r"xdg:config/\x", Some(r"\x"), Some(r"\x")),
            (r"xdg:config\x", None, Some("x")),
            (r"xdg:config/\\?\C:\x", Some(r"\\?\C:\x"), Some(r"\\?\C:\x")),
        ] {
            for (rules, want) in [(PathRules::Posix, posix), (PathRules::Windows, windows)] {
                assert_eq!(
                    parse_xdg_token(input, rules),
                    want.map(|sub| (XdgKind::Config, Some(sub))),
                    "{input:?} under {rules:?}"
                );
            }
        }
    }

    #[test]
    fn tilde_expands_against_home() {
        assert_eq!(expand_path_prefix("~/foo", &home("/h")), "/h/foo");
    }

    #[test]
    fn unknown_xdg_token_passes_through_unchanged() {
        // Runtime is permissive: the freeze turns a typo into an error; here
        // it only must not be silently rewritten.
        assert_eq!(expand_path_prefix("xdg:cofnig", &home("/h")), "xdg:cofnig");
    }

    #[test]
    fn ordinary_path_passes_through_unchanged() {
        assert_eq!(expand_path_prefix("/abs/path", &home("/h")), "/abs/path");
    }

    // Unix-only: the join yields `\h\.cache\foo` on Windows, so the `/foo` tail
    // check no longer holds.
    #[cfg(unix)]
    #[test]
    fn xdg_subpath_is_appended() {
        let env = [("HOME", "/h"), ("XDG_CACHE_HOME", "/c")]
            .into_iter()
            .collect::<EnvVars>();
        assert_eq!(expand_path_prefix("xdg:cache/foo", &env), "/c/foo");
    }

    /// A drive, a share or a namespace head in the sub-path would re-root
    /// the join, so the `xdg:` path is unanswerable and stays as written, as
    /// an unknown kind does.
    // Windows-only: a drive is a filename byte elsewhere, and every root the
    // one root.
    #[cfg(windows)]
    #[test]
    fn xdg_subpath_naming_a_volume_passes_through_unchanged() {
        for input in [
            r"xdg:config/C:\x",
            r"xdg:config/a/C:x",
            r"xdg:config/\\?\C:\x",
            r"xdg:config/\\?\UNC\srv\sh",
            r"xdg:config/\\srv\sh\x",
            r"xdg:config/\\.\pipe\x",
            "xdg:config///x",
        ] {
            assert_eq!(expand_path_prefix(input, &home(r"C:\h")), input);
        }
    }
}
