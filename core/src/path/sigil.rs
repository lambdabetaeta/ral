//! Run-time expansion of the path-prefix sigils `~[/sub]` and `xdg:NAME[/sub]`
//! at the head of a path.  Anything else passes through unchanged.
//!
//! `~` and `xdg:` expand both here, at run time (stage 1 of
//! [`crate::path::Resolver`]), and at policy freeze; the other sigils
//! (`cwd:`, `tempdir:`, `gitdir:`, and the exec-only `path:` and `system:`)
//! are freeze-only, and `guard::freeze` owns them with the rest of the freeze.
//! XDG uses the Linux defaults on every platform, macOS included
//! ([`crate::host`]).

use crate::host::{XdgKind, resolve_xdg};
use crate::path::tilde::TildePath;

/// Parse `xdg:NAME[/sub]`; `None` for a non-`xdg:` input or an unknown name.
pub(crate) fn parse_xdg_token(input: &str) -> Option<(XdgKind, Option<&str>)> {
    let body = input.strip_prefix("xdg:")?;
    let (name, sub) = match body.split_once('/') {
        Some((n, s)) => (n, Some(relative_suffix(s))),
        None => (body, None),
    };
    Some((name.parse().ok()?, sub))
}

/// A sigil's sub-path as something that joins *onto* its base: leading
/// separators come off, because `Path::join` on a rooted suffix discards the
/// base outright — `xdg:data//etc` would otherwise name `/etc`.
pub(crate) fn relative_suffix(s: &str) -> &str {
    s.trim_start_matches(['/', '\\'])
}

/// Expand a `~` or `xdg:` head, or return `input` unchanged.  The runtime half
/// of expansion: no filesystem access, and `home` is both the tilde root and the
/// fallback when an XDG env var is unset.
///
/// Infallible, because [`Resolver::resolve`](super::Resolver::resolve) is:
/// whatever cannot be answered passes through literally — an unknown `home`
/// leaves `~/x` as `~/x`.  That fails closed, a prefix matching nothing
/// beating a fabricated path that might; a caller wanting the same gap to be
/// a configuration error freezes with `guard::freeze::FreezeCtx::path`.
pub fn expand_path_prefix(input: &str, home: Option<&str>) -> String {
    if let Some((kind, sub)) = parse_xdg_token(input) {
        return match resolve_xdg(kind, home) {
            None => input.to_string(),
            Some(base) => match sub {
                None => base.to_string_lossy().into_owned(),
                Some(s) => base.join(s).to_string_lossy().into_owned(),
            },
        };
    }
    if let Some(t) = TildePath::parse(input) {
        return home.map_or_else(|| input.to_string(), |home| t.expand(home));
    }
    input.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let (k, sub) = parse_xdg_token(&token).expect("known kind");
            assert_eq!(k, kind);
            assert!(sub.is_none());
        }
    }

    #[test]
    fn parse_xdg_token_carries_subpath() {
        let (k, sub) = parse_xdg_token("xdg:config/agda/lib").unwrap();
        assert_eq!(k, XdgKind::Config);
        assert_eq!(sub, Some("agda/lib"));
    }

    #[test]
    fn parse_xdg_token_rejects_unknown_name() {
        assert!(parse_xdg_token("xdg:cofnig").is_none());
    }

    #[test]
    fn parse_xdg_token_rejects_non_xdg() {
        assert!(parse_xdg_token("/etc").is_none());
    }

    #[test]
    fn tilde_expands_against_home() {
        assert_eq!(expand_path_prefix("~/foo", Some("/h")), "/h/foo");
    }

    #[test]
    fn unknown_xdg_token_passes_through_unchanged() {
        // Runtime is permissive: the freeze turns a typo into an error; here
        // it only must not be silently rewritten.
        assert_eq!(expand_path_prefix("xdg:cofnig", Some("/h")), "xdg:cofnig");
    }

    #[test]
    fn ordinary_path_passes_through_unchanged() {
        assert_eq!(expand_path_prefix("/abs/path", Some("/h")), "/abs/path");
    }

    // Unix-only: the join yields `\h\.cache\foo` on Windows, so the `/foo` tail
    // check no longer holds.
    #[cfg(unix)]
    #[test]
    fn xdg_subpath_is_appended() {
        // Only the tail is asserted: the base moves with `XDG_CACHE_HOME`.
        let out = expand_path_prefix("xdg:cache/foo", Some("/h"));
        assert!(out.ends_with("/foo"), "got {out}");
    }
}
