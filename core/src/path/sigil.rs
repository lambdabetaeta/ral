//! Path-prefix sigils: `~[user][/sub]`, `xdg:NAME[/sub]`, `cwd:[/sub]`,
//! `tempdir:[/sub]`, and `gitdir:[/sub]` at the head of a grant path.
//!
//! A policy thus names no host's home, XDG layout, or working directory.
//! Anything else passes through unchanged.
//!
//! `~` and `xdg:` expand both at runtime (stage 1 of [`crate::path::Resolver`])
//! and at policy freeze; the other three are freeze-only, and resolve exactly
//! once, so a later `chdir` or `$TMPDIR` change cannot retroactively widen a
//! grant.  XDG uses the Linux defaults on every platform, macOS included
//! ([`crate::path::basedir`]); `gitdir:` follows a worktree `.git` pointer via
//! [`crate::path::discover_git_dir`], falling back to the cwd outside a repo.
//!
//! `path:` and `system:` ([`system_tool_roots`]) are exec-only, each expanding
//! to many directories rather than one path, and `capability::decode`'s
//! exec-map freeze handles them instead of this module.

use crate::path::basedir::{XdgKind, resolve_xdg};
use crate::path::lex::fold_dots;
use crate::path::resolved::NormalizedPrefix;
use crate::path::tilde::{TildePath, expand_tilde_path};
use crate::types::PolicyError;
use std::path::{Path, PathBuf};

/// Shaped like an `xdg:` token, known name or not — so a load-time validator
/// can tell an unknown token from an ordinary path.
pub fn looks_like_xdg(s: &str) -> bool {
    s.starts_with("xdg:")
}

/// A path separator, or one of the five sigil heads [`freeze_one`] knows.
/// `capability::decode`'s exec map uses it to let bare command names (`git`)
/// through freeze unresolved.
pub fn looks_like_path_or_sigil(s: &str) -> bool {
    s.contains('/')
        || s.starts_with('~')
        || s.starts_with("xdg:")
        || s.starts_with("cwd:")
        || s.starts_with("tempdir:")
        || s.starts_with("gitdir:")
}

/// Parse `xdg:NAME[/sub]`; `None` for a non-`xdg:` input or an unknown name.
pub fn parse_xdg_token(input: &str) -> Option<(XdgKind, Option<&str>)> {
    let body = input.strip_prefix("xdg:")?;
    let (name, sub) = match body.split_once('/') {
        Some((n, s)) => (n, Some(s)),
        None => (body, None),
    };
    Some((XdgKind::parse(name)?, sub))
}

/// Expand a `~` or `xdg:` head, or return `input` unchanged.  The runtime half
/// of expansion: no filesystem access, and `home` is both the tilde root and the
/// fallback when an XDG env var is unset.
///
/// Infallible, because [`Resolver::resolve`](super::Resolver::resolve) is: an
/// empty `home` expands `~/x` to `/x`, and a `~user` this platform cannot
/// resolve passes through literally.  Both fail closed — a prefix that matches
/// nothing beats a fabricated path that might — but a caller wanting an unset
/// `$HOME` to be a configuration error must use [`freeze_one`].
pub fn expand_path_prefix(input: &str, home: &str) -> String {
    if let Some((kind, sub)) = parse_xdg_token(input) {
        let base = resolve_xdg(kind, home);
        return match sub {
            None => base.to_string_lossy().into_owned(),
            Some(s) => base.join(s).to_string_lossy().into_owned(),
        };
    }
    if let Some(t) = TildePath::parse(input) {
        return expand_tilde_path(t.user.as_deref(), t.suffix.as_deref(), home)
            .unwrap_or_else(|| input.to_string());
    }
    input.to_string()
}

/// The caller-supplied half of the freeze context; `xdg:` and `tempdir:` read
/// the process environment, and `gitdir:` walks the filesystem from `cwd`.
pub struct FreezeCtx<'a> {
    pub home: &'a str,
    pub cwd: &'a Path,
}

/// [`freeze_one`] over a list, minting the grant's whole prefix set at once.
///
/// Resolving here at load rather than per-check is what makes a grant immune to
/// later env and cwd changes, and closes the window in which `XDG_*_HOME` could
/// mutate between load and a subsequent access.
///
/// # Errors
/// Whatever [`freeze_one`] rejects, on the first entry that does.
pub fn freeze_path_list(
    paths: Vec<String>,
    ctx: &FreezeCtx<'_>,
) -> Result<Vec<NormalizedPrefix>, PolicyError> {
    paths
        .into_iter()
        .map(|entry| freeze_one(&entry, ctx))
        .collect()
}

/// Expand one entry's sigil against `ctx`, then fold `.`/`..` and wrap, so every
/// frozen entry is sigil-free and in the normal form the gate matches against.
///
/// Only `xdg:` carries the under-`home` guard: `$TMPDIR` and a worktree's git
/// directory legitimately live outside `home`, so those two sigils trust their
/// source as given.
///
/// # Errors
/// An unknown `xdg:` token, an `xdg:` path that escapes `$HOME` once folded, an
/// unset `$HOME` under a home-relative sigil (`~`, `xdg:`), or a `~user` naming
/// another user's home off Unix.
#[allow(clippy::disallowed_methods)]
pub fn freeze_one(entry: &str, ctx: &FreezeCtx<'_>) -> Result<NormalizedPrefix, PolicyError> {
    if looks_like_xdg(entry) {
        require_home(ctx)?;
        let (kind, sub) =
            parse_xdg_token(entry).ok_or_else(|| PolicyError::new(unknown_xdg_message(entry)))?;
        return resolve_xdg_safe(kind, sub, ctx.home);
    }
    if let Some(sub) = parse_literal_sigil(entry, "cwd") {
        return Ok(join_sub(ctx.cwd.to_path_buf(), sub));
    }
    if let Some(sub) = parse_literal_sigil(entry, "tempdir") {
        return Ok(join_sub(std::env::temp_dir(), sub));
    }
    if let Some(sub) = parse_literal_sigil(entry, "gitdir") {
        let base = crate::path::discover_git_dir(ctx.cwd).unwrap_or_else(|| ctx.cwd.to_path_buf());
        return Ok(join_sub(base, sub));
    }
    if let Some(t) = TildePath::parse(entry) {
        require_home(ctx)?;
        let expanded = expand_tilde_path(t.user.as_deref(), t.suffix.as_deref(), ctx.home)
            .ok_or_else(|| PolicyError::new(unresolvable_named_user_message(entry)))?;
        return Ok(NormalizedPrefix::freeze(Path::new(&expanded)));
    }
    Ok(NormalizedPrefix::freeze(Path::new(entry)))
}

fn unresolvable_named_user_message(entry: &str) -> String {
    format!(
        "'{entry}' names another user's home directory, which this platform \
         cannot resolve (no getpwnam(3) equivalent) — replace it with an \
         explicit absolute path, or use bare `~`/`~/...` for the current user."
    )
}

fn require_home(ctx: &FreezeCtx<'_>) -> Result<(), PolicyError> {
    if ctx.home.is_empty() {
        return Err(PolicyError::new(
            "HOME is unset, so `~/...` and `xdg:...` tokens in the policy \
             can't be resolved.  Set HOME in the environment, or replace the \
             sigil-bearing entries in the policy with explicit absolute paths.",
        ));
    }
    Ok(())
}

/// Match `name:`, `name:sub`, or `name:/sub`; leading slashes come off the
/// suffix so it joins as a relative component.
#[allow(
    clippy::option_option,
    reason = "tri-state: no-match / match-no-suffix / match-with-suffix"
)]
fn parse_literal_sigil<'a>(input: &'a str, name: &str) -> Option<Option<&'a str>> {
    let body = input.strip_prefix(name)?.strip_prefix(':')?;
    Some(if body.is_empty() {
        None
    } else {
        Some(body.trim_start_matches('/'))
    })
}

fn join_sub(base: PathBuf, sub: Option<&str>) -> NormalizedPrefix {
    let full = match sub {
        None | Some("") => base,
        Some(s) => base.join(s),
    };
    NormalizedPrefix::freeze(&full)
}

/// Resolve an XDG kind plus sub-path, and require the result under `home`:
/// otherwise an attacker-set `XDG_DATA_HOME=/etc` would silently widen an
/// `xdg:data` grant to `/etc`.  Both sides are folded before the comparison, so
/// `xdg:config/../../etc` collapses to `/etc` and is caught at the door instead
/// of stepping over the guard and collapsing only at match time.
#[allow(clippy::disallowed_methods)]
fn resolve_xdg_safe(
    kind: XdgKind,
    sub: Option<&str>,
    home: &str,
) -> Result<NormalizedPrefix, PolicyError> {
    let resolved = join_sub(resolve_xdg(kind, home), sub);
    let folded_home = fold_dots(Path::new(home));
    if resolved.surface_path().starts_with(&folded_home) {
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
    Err(PolicyError::new(format!(
        "xdg:{name} resolves to '{path}', outside HOME — refusing to widen \
         the grant.  {clause}",
        name = kind.token_name(),
        path = resolved.as_str(),
        clause = env_clause,
    )))
}

fn unknown_xdg_message(entry: &str) -> String {
    format!(
        "unknown xdg token '{entry}' — known kinds are: {}. \
         Did you mean one of those? (Token form is `xdg:NAME` or \
         `xdg:NAME/sub/path`.)",
        XdgKind::all().join(", "),
    )
}

/// The platform's tool-root directories — what the `system:` exec sigil expands
/// to.
///
/// Feeds [`unix_tool_roots`] or [`windows_tool_roots`] the live filesystem and
/// environment.  Both stay public and parameterised over those inputs so each
/// platform's list is unit-testable on every host, not only the one compiling
/// it — the pattern `capability::exec`'s `names_match` follows too.
pub fn system_tool_roots() -> Vec<String> {
    #[cfg(windows)]
    {
        let system_root = std::env::var("SystemRoot").unwrap_or_default();
        let program_files = std::env::var("ProgramFiles").unwrap_or_default();
        let program_files_x86 = std::env::var("ProgramFiles(x86)").unwrap_or_default();
        let program_files_dirs: Vec<&str> = [program_files.as_str(), program_files_x86.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        windows_tool_roots(&system_root, &program_files_dirs, crate::path::exists)
    }
    #[cfg(not(windows))]
    {
        unix_tool_roots(crate::path::exists)
    }
}

/// `/usr/bin` and `/bin` unconditionally, plus whichever Homebrew prefix
/// `exists` reports.
///
/// That is `/opt/homebrew` as `sandbox::macos` admits for `Exec` and
/// `/home/linuxbrew/.linuxbrew` as `sandbox::linux` lists, mirrored here for
/// the capability layer's separate exec-admission concern.
pub fn unix_tool_roots(exists: impl Fn(&str) -> bool) -> Vec<String> {
    let mut roots = vec!["/usr/bin".to_string(), "/bin".to_string()];
    for brew in ["/opt/homebrew", "/home/linuxbrew/.linuxbrew"] {
        if exists(brew) {
            roots.push(brew.to_string());
        }
    }
    roots
}

/// `%SystemRoot%\System32` and the bundled Windows PowerShell home — falling
/// back to the conventional `C:\Windows` when `system_root` is empty.
///
/// A Git-for-Windows `usr\bin` joins them, under whichever
/// `program_files_dirs` entry `exists` reports.
pub fn windows_tool_roots(
    system_root: &str,
    program_files_dirs: &[&str],
    exists: impl Fn(&str) -> bool,
) -> Vec<String> {
    let system_root = if system_root.is_empty() {
        r"C:\Windows"
    } else {
        system_root
    };
    let mut roots = vec![
        format!(r"{system_root}\System32"),
        format!(r"{system_root}\System32\WindowsPowerShell\v1.0"),
    ];
    for pf in program_files_dirs {
        let git_bin = format!(r"{pf}\Git\usr\bin");
        if exists(&git_bin) {
            roots.push(git_bin);
        }
    }
    roots
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn ctx<'a>(home: &'a str, cwd: &'a Path) -> FreezeCtx<'a> {
        FreezeCtx { home, cwd }
    }

    fn frozen(paths: &[&str], ctx: &FreezeCtx<'_>) -> Result<Vec<String>, PolicyError> {
        freeze_path_list(
            paths.iter().map(std::string::ToString::to_string).collect(),
            ctx,
        )
        .map(|v| v.iter().map(|p| p.as_str().to_string()).collect())
    }

    // Unix-only: `PathBuf::join` yields `\` separators on Windows.
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
        // macOS `$TMPDIR` ends in `/`, which folding strips — so compare
        // against the same kernel rather than a literal.
        let temp = std::env::temp_dir();
        let fold = |p: &Path| fold_dots(p).to_string_lossy().into_owned();
        assert_eq!(paths[0], fold(&temp));
        assert_eq!(paths[1], fold(&temp.join("scratch")));
    }

    #[test]
    fn freeze_leaves_literal_paths_alone() {
        let paths = frozen(&["/tmp", "/etc/hosts"], &ctx("/h", Path::new("/cwd"))).unwrap();
        // `fold_dots` rebuilds with the host separator, so the frozen form is
        // `\tmp` on Windows — compare against the same kernel.
        let fold = |s: &str| fold_dots(Path::new(s)).to_string_lossy().into_owned();
        assert_eq!(paths, vec![fold("/tmp"), fold("/etc/hosts")]);
    }

    /// The guard folds the whole prefix before comparing, so a `..` climb out
    /// of HOME is rejected at freeze rather than collapsing at match time.
    // Unix-only: the folded escape `/etc` is a Unix root.
    #[cfg(unix)]
    #[test]
    fn freeze_rejects_xdg_subpath_escaping_home() {
        let err = frozen(
            &["xdg:config/../../../../etc"],
            &ctx("/h", Path::new("/cwd")),
        )
        .unwrap_err()
        .message;
        assert!(err.contains("outside HOME"), "{err}");
    }

    /// Even a sigil-free literal is stored in the form the gate matches against.
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

    /// No `getpwnam(3)` off Unix, and `expand_path_prefix` cannot fail, so the
    /// literal spelling survives rather than a fabricated path.
    #[cfg(not(unix))]
    #[test]
    fn named_user_tilde_passes_through_unchanged_off_unix() {
        assert_eq!(expand_path_prefix("~bob/foo", "/h"), "~bob/foo");
    }

    /// `freeze_one` can fail, so the same entry is a load-time error rather
    /// than a frozen grant that can never match.
    #[cfg(not(unix))]
    #[test]
    fn freeze_rejects_named_user_tilde_off_unix() {
        let err = frozen(&["~bob/secrets"], &ctx("/h", Path::new("/cwd")))
            .unwrap_err()
            .message;
        assert!(err.contains("~bob/secrets"), "{err}");
    }

    #[test]
    fn unknown_xdg_token_passes_through_unchanged() {
        // Runtime is permissive — the load-time validator turns a typo into an
        // error; here it only must not be silently rewritten.
        assert_eq!(expand_path_prefix("xdg:cofnig", "/h"), "xdg:cofnig");
    }

    #[test]
    fn ordinary_path_passes_through_unchanged() {
        assert_eq!(expand_path_prefix("/abs/path", "/h"), "/abs/path");
    }

    // Unix-only: the join yields `\h\.cache\foo` on Windows, so the `/foo` tail
    // check no longer holds.
    #[cfg(unix)]
    #[test]
    fn xdg_subpath_is_appended() {
        // Only the tail is asserted: the base moves with `$XDG_CACHE_HOME`.
        let out = expand_path_prefix("xdg:cache/foo", "/h");
        assert!(out.ends_with("/foo"), "got {out}");
    }

    #[test]
    fn unix_tool_roots_always_carries_usr_bin_and_bin() {
        let roots = unix_tool_roots(|_| false);
        assert_eq!(roots, vec!["/usr/bin".to_string(), "/bin".to_string()]);
    }

    #[test]
    fn unix_tool_roots_adds_present_homebrew_prefix_only() {
        let roots = unix_tool_roots(|p| p == "/opt/homebrew");
        assert_eq!(
            roots,
            vec![
                "/usr/bin".to_string(),
                "/bin".to_string(),
                "/opt/homebrew".to_string(),
            ]
        );
    }

    #[test]
    fn unix_tool_roots_ignores_absent_homebrew_prefixes() {
        let roots = unix_tool_roots(|_| false);
        assert!(!roots.iter().any(|r| r.contains("homebrew")));
    }

    /// No `cfg(windows)` on any of the Windows-shape tests: they run on the
    /// macOS and Linux CI hosts that never compile `system_tool_roots`' other
    /// half.
    #[test]
    fn windows_tool_roots_always_carries_system32_and_powershell() {
        let roots = windows_tool_roots(r"C:\Windows", &[], |_| false);
        assert_eq!(
            roots,
            vec![
                r"C:\Windows\System32".to_string(),
                r"C:\Windows\System32\WindowsPowerShell\v1.0".to_string(),
            ]
        );
    }

    #[test]
    fn windows_tool_roots_falls_back_when_system_root_unset() {
        let roots = windows_tool_roots("", &[], |_| false);
        assert!(roots[0].starts_with(r"C:\Windows"), "got {roots:?}");
    }

    #[test]
    fn windows_tool_roots_adds_git_for_windows_usr_bin_when_present() {
        let roots = windows_tool_roots(r"C:\Windows", &[r"C:\Program Files"], |p| {
            p == r"C:\Program Files\Git\usr\bin"
        });
        assert!(
            roots.contains(&r"C:\Program Files\Git\usr\bin".to_string()),
            "got {roots:?}"
        );
    }

    #[test]
    fn windows_tool_roots_omits_git_for_windows_when_absent() {
        let roots = windows_tool_roots(r"C:\Windows", &[r"C:\Program Files"], |_| false);
        assert!(!roots.iter().any(|r| r.contains("Git")), "got {roots:?}");
    }

    #[test]
    fn windows_tool_roots_checks_both_program_files_locations() {
        let roots = windows_tool_roots(
            r"C:\Windows",
            &[r"C:\Program Files", r"C:\Program Files (x86)"],
            |p| p == r"C:\Program Files (x86)\Git\usr\bin",
        );
        assert!(
            roots.contains(&r"C:\Program Files (x86)\Git\usr\bin".to_string()),
            "got {roots:?}"
        );
    }
}
