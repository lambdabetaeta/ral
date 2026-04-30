//! Expansion of path-prefix sigils for grant paths.
//!
//! Four sigils are recognised at the head of a path string:
//!
//!   * `~` / `~/...` / `~user[/...]` — to a home directory, the
//!     usual shell tilde rule.
//!   * `xdg:NAME[/sub]` — to an XDG basedir, honouring the
//!     `XDG_*_HOME` env vars with the Linux defaults universally
//!     (so `xdg:config` is `~/.config` everywhere — no
//!     `~/Library/Application Support` substitution on macOS,
//!     matching how cross-platform CLI tools and dotfiles use XDG).
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

use crate::path::tilde::{TildePath, expand_tilde_path};
use std::path::{Path, PathBuf};

/// One of the XDG basedir kinds we expose as `xdg:NAME` tokens.
///
/// `Config`, `Data`, `Cache`, `State` follow the [XDG basedir spec].
/// `Bin` is non-spec but conventional: many dotfiles set
/// `XDG_BIN_HOME=$HOME/.local/bin` and we honour it the same way.
///
/// [XDG basedir spec]: https://specifications.freedesktop.org/basedir-spec/basedir-spec-latest.html
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XdgKind {
    Config,
    Data,
    Cache,
    State,
    Bin,
}

impl XdgKind {
    /// Parse the `NAME` part of an `xdg:NAME` token.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "config" => Some(Self::Config),
            "data"   => Some(Self::Data),
            "cache"  => Some(Self::Cache),
            "state"  => Some(Self::State),
            "bin"    => Some(Self::Bin),
            _ => None,
        }
    }

    /// All known kinds, in the canonical order.  Used by error
    /// messages so a typo lists the alternatives.
    pub fn all() -> &'static [&'static str] {
        &["config", "data", "cache", "state", "bin"]
    }

    /// Lower-case `NAME` form, the way authors write it in policy.
    pub fn token_name(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Data   => "data",
            Self::Cache  => "cache",
            Self::State  => "state",
            Self::Bin    => "bin",
        }
    }

    /// The env var that overrides the default for this kind, for
    /// inclusion in error messages.  `bin` is non-spec but
    /// `XDG_BIN_HOME` is the conventional override.
    fn env_var(self) -> &'static str {
        match self {
            Self::Config => "XDG_CONFIG_HOME",
            Self::Data   => "XDG_DATA_HOME",
            Self::Cache  => "XDG_CACHE_HOME",
            Self::State  => "XDG_STATE_HOME",
            Self::Bin    => "XDG_BIN_HOME",
        }
    }
}

/// True when `s` looks like an `xdg:` token, regardless of whether
/// the name is one we recognise.  Load-time validators use this
/// to distinguish "unknown token" from "ordinary path".
pub fn looks_like_xdg(s: &str) -> bool {
    s.starts_with("xdg:")
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

/// Resolve an XDG kind to its absolute filesystem path.
///
/// Reads the corresponding `XDG_*_HOME` env var if it holds an
/// absolute path; otherwise falls back to `home` joined with the
/// spec's default suffix.  Both routes share the same `home`
/// argument so a `within [shell: HOME=…]` override flows through
/// to xdg sigils with the same semantics as tilde sigils — no
/// detour through a separate process-level HOME lookup.
///
/// The XDG spec rule "relative values are ignored" is encoded in
/// [`absolute_env_var`].
fn resolve_xdg(kind: XdgKind, home: &str) -> PathBuf {
    let home_path = Path::new(home);
    let default_suffix = match kind {
        XdgKind::Config => ".config",
        XdgKind::Data   => ".local/share",
        XdgKind::Cache  => ".cache",
        XdgKind::State  => ".local/state",
        XdgKind::Bin    => ".local/bin",
    };
    absolute_env_var(kind.env_var()).unwrap_or_else(|| home_path.join(default_suffix))
}

/// Read an env var and return it as a `PathBuf` only if it parses
/// as an absolute path — matches the XDG spec's rule that relative
/// values are ignored.
fn absolute_env_var(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// Per-call inputs for the freeze pass.  `home` and `cwd` are
/// supplied by the caller; `tempdir` is read from the process env
/// (`std::env::temp_dir`) the same way XDG sigils read
/// `XDG_*_HOME`.  Bundled rather than passed positionally so new
/// sigils can grow this struct without rippling through callers.
pub struct FreezeCtx<'a> {
    pub home: &'a str,
    pub cwd: &'a Path,
}

/// Reject any policy-level sigil token that does not name a known
/// kind (`xdg:`, `cwd:`, `tempdir:`).  Pure syntactic check; no
/// env or filesystem access.  Useful in tests of pure data;
/// production loaders should call [`freeze_path_list`] which
/// subsumes this check and does the safety-validating resolution.
pub fn validate_xdg_tokens(paths: &[String]) -> Result<(), String> {
    for p in paths {
        if looks_like_xdg(p) && parse_xdg_token(p).is_none() {
            return Err(unknown_xdg_message(p));
        }
    }
    Ok(())
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
/// Resolution is one-shot: after `freeze_path_list` succeeds the
/// list contains no sigils, so subsequent grant matching reads
/// concrete paths and ignores later env or cwd changes.
pub fn freeze_path_list(paths: &mut [String], ctx: &FreezeCtx<'_>) -> Result<(), String> {
    if ctx.home.is_empty() {
        return Err(
            "HOME is unset, so `~/...` and `xdg:...` tokens in the policy \
             can't be resolved.  Set HOME in the environment, or replace the \
             sigil-bearing entries in the policy with explicit absolute paths."
                .into(),
        );
    }
    for entry in paths.iter_mut() {
        if let Some(frozen) = freeze_one(entry, ctx)? {
            *entry = frozen;
        }
    }
    Ok(())
}

/// Resolve sigils in a single entry.  Returns `Ok(None)` when the
/// entry has no sigil to expand (caller leaves it untouched), the
/// expanded form on success, or a descriptive error otherwise.
fn freeze_one(entry: &str, ctx: &FreezeCtx<'_>) -> Result<Option<String>, String> {
    if looks_like_xdg(entry) {
        let (kind, sub) = parse_xdg_token(entry).ok_or_else(|| unknown_xdg_message(entry))?;
        let base = resolve_xdg_safe(kind, ctx.home)?;
        return Ok(Some(join_sub(base, sub)));
    }
    if let Some(sub) = parse_literal_sigil(entry, "cwd") {
        return Ok(Some(join_sub(ctx.cwd.to_path_buf(), sub)));
    }
    if let Some(sub) = parse_literal_sigil(entry, "tempdir") {
        return Ok(Some(join_sub(std::env::temp_dir(), sub)));
    }
    if let Some(t) = TildePath::parse(entry) {
        return Ok(Some(expand_tilde_path(t.user.as_deref(), t.suffix.as_deref(), ctx.home)));
    }
    Ok(None)
}

/// Match `name:` or `name:/sub` and return the optional sub-path.
/// Used for `cwd:` and `tempdir:` — sigils whose only structure is
/// an optional `/sub` suffix (no env var, no kind enum).
fn parse_literal_sigil<'a>(input: &'a str, name: &str) -> Option<Option<&'a str>> {
    let body = input.strip_prefix(name)?.strip_prefix(':')?;
    Some(if body.is_empty() { None } else { Some(body.trim_start_matches('/')) })
}

/// `base` joined with the (possibly empty) sub-path, rendered to a
/// String.  Shared tail of every sigil expansion in `freeze_one`.
fn join_sub(base: PathBuf, sub: Option<&str>) -> String {
    let full = match sub {
        None | Some("") => base,
        Some(s) => base.join(s),
    };
    full.to_string_lossy().into_owned()
}

/// Resolve an XDG kind and verify the result is a subpath of
/// `home`.  Errors when an `XDG_*_HOME` override sends the
/// resolved path outside `home`, naming the env var and its value
/// so the operator can see exactly what to fix.
fn resolve_xdg_safe(kind: XdgKind, home: &str) -> Result<PathBuf, String> {
    let resolved = resolve_xdg(kind, home);
    if resolved.starts_with(home) {
        return Ok(resolved);
    }
    let val = std::env::var(kind.env_var()).unwrap_or_default();
    let env_clause = if val.is_empty() {
        format!(
            "{var} is unset, so the default ({}) was used — is HOME ({home}) \
             set correctly?",
            resolved.display(),
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
        path = resolved.display(),
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
mod tests {
    use super::*;

    fn ctx<'a>(home: &'a str, cwd: &'a Path) -> FreezeCtx<'a> {
        FreezeCtx { home, cwd }
    }

    #[test]
    fn freeze_expands_cwd_sigil() {
        let mut paths = vec!["cwd:".into(), "cwd:/src".into()];
        freeze_path_list(&mut paths, &ctx("/h", Path::new("/work/proj"))).unwrap();
        assert_eq!(paths, vec!["/work/proj".to_string(), "/work/proj/src".to_string()]);
    }

    #[test]
    fn freeze_expands_tempdir_sigil() {
        let mut paths = vec!["tempdir:".into(), "tempdir:/scratch".into()];
        freeze_path_list(&mut paths, &ctx("/h", Path::new("/cwd"))).unwrap();
        let temp = std::env::temp_dir();
        assert_eq!(paths[0], temp.to_string_lossy().to_string());
        assert_eq!(paths[1], temp.join("scratch").to_string_lossy().to_string());
    }

    #[test]
    fn freeze_leaves_literal_paths_alone() {
        // The old "/tmp + tempdir + cwd" runtime injection used to
        // overwrite literals; sigils are opt-in and must not.
        let mut paths = vec!["/tmp".into(), "/etc/hosts".into()];
        freeze_path_list(&mut paths, &ctx("/h", Path::new("/cwd"))).unwrap();
        assert_eq!(paths, vec!["/tmp".to_string(), "/etc/hosts".to_string()]);
    }

    #[test]
    fn parse_xdg_token_recognises_each_kind() {
        for (name, kind) in [
            ("config", XdgKind::Config),
            ("data",   XdgKind::Data),
            ("cache",  XdgKind::Cache),
            ("state",  XdgKind::State),
            ("bin",    XdgKind::Bin),
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

    #[test]
    fn xdg_subpath_is_appended() {
        // The base resolves against the env or `home`, but the
        // user-provided suffix is fixed.
        let out = expand_path_prefix("xdg:cache/foo", "/h");
        assert!(out.ends_with("/foo"), "got {out}");
    }
}
