//! Expansion of path-prefix sigils for grant paths.
//!
//! Four sigils are recognised at the head of a path string:
//!
//!   * `~` / `~/...` / `~user[/...]` — to a home directory, the
//!     usual shell tilde rule.
//!   * `xdg:NAME[/sub]` — to an XDG basedir, resolved by
//!     [`crate::path::basedir`] (Linux defaults universally, so
//!     `xdg:config` is `~/.config` everywhere — no
//!     `~/Library/Application Support` substitution on macOS).
//!   * `cwd:[/sub]` — to the working directory at policy freeze.
//!     Resolved exactly once: the grant remembers where it was
//!     created, so a later `chdir` cannot retroactively widen
//!     authority.
//!   * `tempdir:[/sub]` — to `std::env::temp_dir()`, the platform's
//!     scratch directory (`$TMPDIR` on macOS, `/tmp` on Linux).
//!     Distinct from a literal `"/tmp"` because macOS rarely uses it.
//!
//! Anything else passes through unchanged.  `~` and `xdg:` work
//! both at runtime (stage 1 of [`crate::path::Resolver`]) and at
//! policy freeze; `cwd:` and `tempdir:` are policy-only and only
//! the freeze pass expands them.  Policy authors thus write
//! portable paths in `.exarch.toml` and `grant { fs: ... }`
//! blocks without naming the host's home directory, XDG layout,
//! or working directory directly.

use crate::path::basedir::{XdgKind, resolve_xdg};
use crate::path::lex::fold_dots;
use crate::path::resolved::NormalizedPrefix;
use crate::path::tilde::{TildePath, expand_tilde_path};
use std::path::{Path, PathBuf};

/// True when `s` looks like an `xdg:` token, regardless of whether
/// the name is one we recognise.  Load-time validators use this
/// to distinguish "unknown token" from "ordinary path".
pub fn looks_like_xdg(s: &str) -> bool {
    s.starts_with("xdg:")
}

/// True when `s` is shaped like a path or path-prefix sigil — it
/// either contains a path separator or starts with one of the four
/// sigil tokens recognised by [`freeze_one`] (`~`, `xdg:`, `cwd:`,
/// `tempdir:`, `gitdir:`).  Used by the unified exec map: bare command names
/// (no `/`, no sigil) pass through freeze unchanged; everything else
/// gets sigil-resolved.
pub fn looks_like_path_or_sigil(s: &str) -> bool {
    s.contains('/')
        || s.starts_with('~')
        || s.starts_with("xdg:")
        || s.starts_with("cwd:")
        || s.starts_with("tempdir:")
        || s.starts_with("gitdir:")
}

/// Parse `xdg:NAME[/sub]` into a kind plus optional sub-path.
/// `None` if the input does not start with `xdg:` or names an
/// unknown kind.
pub fn parse_xdg_token(input: &str) -> Option<(XdgKind, Option<&str>)> {
    let body = input.strip_prefix("xdg:")?;
    let (name, sub) = match body.split_once('/') {
        Some((n, s)) => (n, Some(s)),
        None => (body, None),
    };
    Some((XdgKind::parse(name)?, sub))
}

/// Expand a path-prefix sigil if present, otherwise return the
/// input unchanged.  Pure once `home` is fixed: no filesystem
/// access.
///
/// `home` is the directory used for tilde expansion and as the
/// fallback root when an XDG env var is unset.  The XDG env vars
/// themselves are read from the process environment.
pub fn expand_path_prefix(input: &str, home: &str) -> String {
    if let Some((kind, sub)) = parse_xdg_token(input) {
        let base = resolve_xdg(kind, home);
        return match sub {
            None => base.to_string_lossy().into_owned(),
            Some(s) => base.join(s).to_string_lossy().into_owned(),
        };
    }
    if let Some(t) = TildePath::parse(input) {
        return expand_tilde_path(t.user.as_deref(), t.suffix.as_deref(), home);
    }
    input.to_string()
}

/// Per-call inputs for the freeze pass.  `home` and `cwd` are
/// supplied by the caller; `tempdir` is read from the process env
/// (`std::env::temp_dir`) the same way XDG sigils read
/// `XDG_*_HOME`; `gitdir:` resolves via [`discover_git_dir`]
/// (which walks the filesystem).  Bundled rather than passed
/// positionally so new sigils can grow this struct without
/// rippling through callers.
pub struct FreezeCtx<'a> {
    pub home: &'a str,
    pub cwd: &'a Path,
}

/// Resolve every sigil-bearing entry in `paths` against `ctx`,
/// rewriting it in place.  Tilde paths expand against `home`;
/// `xdg:NAME[/sub]` resolves via the XDG env vars (and is required
/// to land under `home`); `cwd:[/sub]` resolves to `ctx.cwd`;
/// `tempdir:[/sub]` resolves to `std::env::temp_dir()`.
///
/// The under-`home` check on XDG is defence in depth against
/// attacker-controlled `XDG_*_HOME` widening the grant: with
/// `XDG_DATA_HOME=/etc` set in the calling process, an
/// `xdg:data` entry would otherwise silently grant `/etc` read.
/// Resolving once at load (rather than per-check) also closes the
/// time-of-check-to-time-of-use race where the env mutates between
/// load and a later access.
///
/// Resolution is one-shot: after `freeze_path_list` succeeds every
/// entry is a [`NormalizedPrefix`] — sigil-free, absolute, and
/// `.`/`..`-collapsed — so subsequent grant matching reads concrete
/// paths in the same normal form a [`ResolvedPath`](crate::path::ResolvedPath)
/// carries, ignoring later env or cwd changes.
pub fn freeze_path_list(
    paths: Vec<String>,
    ctx: &FreezeCtx<'_>,
) -> Result<Vec<NormalizedPrefix>, String> {
    paths
        .into_iter()
        .map(|entry| freeze_one(&entry, ctx))
        .collect()
}

/// Freeze one entry into a [`NormalizedPrefix`]: expand any sigil
/// against `ctx`, then fold `.`/`..` and wrap.  Tilde paths expand
/// against `home`; `xdg:NAME[/sub]` resolves via the XDG env vars (and
/// is required to land under `home` *after folding*, closing the
/// `xdg:config/../../etc` escape); `cwd:[/sub]` resolves to `ctx.cwd`;
/// `tempdir:[/sub]` resolves to `std::env::temp_dir()`.  A sigil-free
/// entry is folded and wrapped verbatim.  An unset HOME is a
/// configuration error for the two home-relative sigils.
#[allow(clippy::disallowed_methods)]
pub fn freeze_one(entry: &str, ctx: &FreezeCtx<'_>) -> Result<NormalizedPrefix, String> {
    if looks_like_xdg(entry) {
        require_home(ctx)?;
        let (kind, sub) = parse_xdg_token(entry).ok_or_else(|| unknown_xdg_message(entry))?;
        return resolve_xdg_safe(kind, sub, ctx.home);
    }
    if let Some(sub) = parse_literal_sigil(entry, "cwd") {
        return Ok(join_sub(ctx.cwd.to_path_buf(), sub));
    }
    if let Some(sub) = parse_literal_sigil(entry, "tempdir") {
        return Ok(join_sub(std::env::temp_dir(), sub));
    }
    if let Some(sub) = parse_literal_sigil(entry, "gitdir") {
        let base = crate::path::discover_git_dir(ctx.cwd)
            .unwrap_or_else(|| ctx.cwd.to_path_buf());
        return Ok(join_sub(base, sub));
    }
    if let Some(t) = TildePath::parse(entry) {
        require_home(ctx)?;
        let expanded = expand_tilde_path(t.user.as_deref(), t.suffix.as_deref(), ctx.home);
        return Ok(NormalizedPrefix::freeze(Path::new(&expanded)));
    }
    Ok(NormalizedPrefix::freeze(Path::new(entry)))
}

/// The two home-relative sigils (`~`, `xdg:`) cannot resolve without a
/// HOME; surface the configuration error rather than silently expanding
/// against an empty root.
fn require_home(ctx: &FreezeCtx<'_>) -> Result<(), String> {
    if ctx.home.is_empty() {
        return Err(
            "HOME is unset, so `~/...` and `xdg:...` tokens in the policy \
             can't be resolved.  Set HOME in the environment, or replace the \
             sigil-bearing entries in the policy with explicit absolute paths."
                .into(),
        );
    }
    Ok(())
}

/// Match `name:`, `name:sub`, or `name:/sub` and return the
/// optional sub-path; any leading slashes on the suffix are trimmed
/// so the result joins as a relative component.  Used for `cwd:`
/// and `tempdir:` — sigils whose only structure is an optional
/// suffix (no env var, no kind enum).
fn parse_literal_sigil<'a>(input: &'a str, name: &str) -> Option<Option<&'a str>> {
    let body = input.strip_prefix(name)?.strip_prefix(':')?;
    Some(if body.is_empty() {
        None
    } else {
        Some(body.trim_start_matches('/'))
    })
}

/// `base` joined with the (possibly empty) sub-path, folded and frozen.
/// Shared tail of every sigil expansion in [`freeze_one`].
fn join_sub(base: PathBuf, sub: Option<&str>) -> NormalizedPrefix {
    let full = match sub {
        None | Some("") => base,
        Some(s) => base.join(s),
    };
    NormalizedPrefix::freeze(&full)
}

/// Resolve an XDG kind plus sub-path, fold, and verify the full result
/// is a subpath of `home`.  Both sides are folded before the
/// comparison, so `xdg:config/../../etc` (or `XDG_DATA_HOME=$HOME/../../etc`)
/// collapses to `/etc` — which does not start with `home` — and is
/// rejected at the door rather than escaping the guard and collapsing
/// only at match time.  Errors name the env var and its value so the
/// operator can see exactly what to fix.
#[allow(clippy::disallowed_methods)]
fn resolve_xdg_safe(
    kind: XdgKind,
    sub: Option<&str>,
    home: &str,
) -> Result<NormalizedPrefix, String> {
    let resolved = join_sub(resolve_xdg(kind, home), sub);
    let folded_home = fold_dots(Path::new(home));
    if resolved.as_path().starts_with(&folded_home) {
        return Ok(resolved);
    }
    let val = std::env::var(kind.env_var()).unwrap_or_default();
    let env_clause = if val.is_empty() {
        format!(
            "{var} is unset, so the default ({}) was used — is HOME ({home}) \
             set correctly?",
            resolved.as_str(),
            var = kind.env_var(),
            home = home,
        )
    } else {
        format!(
            "{var}={val} — set it to a subpath of HOME ({home}), unset it to \
             use the default, or replace xdg:{name} in the policy with an \
             explicit path.",
            var = kind.env_var(),
            val = val,
            home = home,
            name = kind.token_name(),
        )
    };
    Err(format!(
        "xdg:{name} resolves to '{path}', outside HOME — refusing to widen \
         the grant.  {clause}",
        name = kind.token_name(),
        path = resolved.as_str(),
        clause = env_clause,
    ))
}

fn unknown_xdg_message(entry: &str) -> String {
    format!(
        "unknown xdg token '{entry}' — known kinds are: {}. \
         Did you mean one of those? (Token form is `xdg:NAME` or \
         `xdg:NAME/sub/path`.)",
        XdgKind::all().join(", "),
    )
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn ctx<'a>(home: &'a str, cwd: &'a Path) -> FreezeCtx<'a> {
        FreezeCtx { home, cwd }
    }

    fn frozen(paths: &[&str], ctx: &FreezeCtx<'_>) -> Result<Vec<String>, String> {
        freeze_path_list(paths.iter().map(|s| s.to_string()).collect(), ctx)
            .map(|v| v.iter().map(|p| p.as_str().to_string()).collect())
    }

    // Unix-only: `cwd:/src` joins via `PathBuf`, producing
    // `/work/proj\src` on Windows.  The grant subsystem is Unix-only.
    #[cfg(unix)]
    #[test]
    fn freeze_expands_cwd_sigil() {
        let paths = frozen(&["cwd:", "cwd:/src"], &ctx("/h", Path::new("/work/proj"))).unwrap();
        assert_eq!(
            paths,
            vec!["/work/proj".to_string(), "/work/proj/src".to_string()]
        );
    }

    #[test]
    fn freeze_expands_tempdir_sigil() {
        let paths = frozen(
            &["tempdir:", "tempdir:/scratch"],
            &ctx("/h", Path::new("/cwd")),
        )
        .unwrap();
        // The frozen forms are folded, so any trailing separator
        // `std::env::temp_dir()` carries (macOS `$TMPDIR` ends in `/`)
        // is normalised away — compare against the same kernel.
        let temp = std::env::temp_dir();
        let fold = |p: &Path| fold_dots(p).to_string_lossy().into_owned();
        assert_eq!(paths[0], fold(&temp));
        assert_eq!(paths[1], fold(&temp.join("scratch")));
    }

    #[test]
    fn freeze_leaves_literal_paths_alone() {
        // Sigils are opt-in; a sigil-free literal is frozen verbatim
        // (only `.`/`..` would fold, and there are none here).
        let paths = frozen(&["/tmp", "/etc/hosts"], &ctx("/h", Path::new("/cwd"))).unwrap();
        assert_eq!(paths, vec!["/tmp".to_string(), "/etc/hosts".to_string()]);
    }

    /// Security regression (bug a): an `xdg:` sub-path that climbs out of
    /// HOME with `..` must be rejected at freeze — the guard folds the
    /// FULL prefix before comparing, so `xdg:config/../../../../etc`
    /// collapses to `/etc`, which does not start with HOME.
    // Unix-only: Unix path shapes; the folded escape `/etc` is a Unix root.
    #[cfg(unix)]
    #[test]
    fn freeze_rejects_xdg_subpath_escaping_home() {
        let err = frozen(
            &["xdg:config/../../../../etc"],
            &ctx("/h", Path::new("/cwd")),
        )
        .unwrap_err();
        assert!(err.contains("outside HOME"), "{err}");
    }

    /// A `.`/`..` in a sigil-free literal folds to its normal form, so
    /// the stored prefix is the same form the gate would match against.
    // Unix-only: Unix path shapes.
    #[cfg(unix)]
    #[test]
    fn freeze_folds_dot_dot_in_literal() {
        let paths = frozen(&["/a/b/../c"], &ctx("/h", Path::new("/cwd"))).unwrap();
        assert_eq!(paths, vec!["/a/c".to_string()]);
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
        assert!(looks_like_xdg("xdg:cofnig"));
        assert!(parse_xdg_token("xdg:cofnig").is_none());
    }

    #[test]
    fn parse_xdg_token_rejects_non_xdg() {
        assert!(!looks_like_xdg("/etc"));
        assert!(parse_xdg_token("/etc").is_none());
    }

    #[test]
    fn tilde_expands_against_home() {
        assert_eq!(expand_path_prefix("~/foo", "/h"), "/h/foo");
    }

    #[test]
    fn unknown_xdg_token_passes_through_unchanged() {
        // Runtime is permissive; the load-time validator is what
        // turns this into an error.  Here we only check that a
        // typo isn't silently rewritten.
        assert_eq!(expand_path_prefix("xdg:cofnig", "/h"), "xdg:cofnig");
    }

    #[test]
    fn ordinary_path_passes_through_unchanged() {
        assert_eq!(expand_path_prefix("/abs/path", "/h"), "/abs/path");
    }

    // Unix-only: the joined base produces backslashes on Windows
    // (`\h\.cache\foo`), so the `/foo` tail check no longer holds.
    #[cfg(unix)]
    #[test]
    fn xdg_subpath_is_appended() {
        // The base resolves against the env or `home`, but the
        // user-provided suffix is fixed.
        let out = expand_path_prefix("xdg:cache/foo", "/h");
        assert!(out.ends_with("/foo"), "got {out}");
    }
}
