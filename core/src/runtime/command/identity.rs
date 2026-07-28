//! What command did the user name?
//!
//! [`CommandIdentity`] freezes the answer at dispatch — the surface
//! spelling we show and the PATH-walked form we exec — and is threaded
//! down to launch, so classification and spawn cannot disagree and PATH
//! is walked once.  Resolution is total: a bare name that misses on PATH
//! keeps `resolved == shown`, leaving the 127/126 verdict to `super::vet`.

use crate::ir::CommandName;
use crate::path::tilde::expand_tilde_path;
use crate::types::Context;

#[derive(Clone, Debug)]
pub(crate) struct CommandIdentity {
    pub(crate) name: CommandName,
    pub(crate) shown: String,
    pub(crate) resolved: String,
}

impl CommandIdentity {
    /// Render and PATH-resolve `name` against `ctx`.
    pub(crate) fn resolve(name: CommandName, ctx: &Context) -> Self {
        let shown = render(&name, ctx);
        let resolved = walk_path(&name, ctx);
        Self {
            name,
            shown,
            resolved,
        }
    }

    /// The narrow identity set: candidate names by which an `exec` grant may
    /// *admit* this head.  [`deny_names_from`](Self::deny_names_from) is the
    /// broad counterpart, for vetoes.
    ///
    /// A `Bare` head drops its surface name whenever a scoped `PATH` redirects
    /// resolution away from the host `PATH`, so an outer grant keyed on the
    /// bare name cannot admit a binary planted on a temporary search path.  A
    /// `Path` head gains the cwd-joined absolute form, because `allow_dirs` and
    /// `deny_dirs` only match absolute candidates.
    pub(crate) fn policy_names(&self, ctx: &Context) -> Vec<String> {
        let mut names = Vec::new();
        let mut include_rendered = true;
        if matches!(self.name, CommandName::Bare(_)) {
            // With equal PATHs the baseline walk is `self.resolved` by
            // construction, so the guard skips it.  That skip is the point:
            // `policy_names` runs several times per command, and on Windows a
            // walk is a filesystem probe per PATHEXT suffix in every PATH dir.
            let host_path = std::env::var("PATH").ok();
            let effective_path = ctx.env_overrides().get_or_host("PATH");
            if host_path != effective_path {
                let baseline = host_path.as_deref().and_then(|path| {
                    crate::path::resolve_in_path(&self.shown, path, ctx.dir.as_deref())
                });
                if baseline.as_deref() != Some(self.resolved.as_str())
                    && self.resolved != self.shown
                {
                    include_rendered = false;
                }
            }
        }
        if include_rendered {
            names.push(self.shown.clone());
        }
        if self.resolved != names.last().map(String::as_str).unwrap_or_default() {
            names.push(self.resolved.clone());
        }
        if matches!(self.name, CommandName::Path(_))
            && let Some(last) = names.last()
            && let Some(abs) = absolutize(last, ctx)
            && !names.iter().any(|n| n == &abs)
        {
            names.push(abs);
        }
        names
    }

    /// The broad identity set: `names`, a fresh
    /// [`policy_names`](Self::policy_names), widened with the basenames of the
    /// resolved and as-invoked forms.  Only a `Deny` consults it.
    ///
    /// A veto must see through both of a command's identities, admission
    /// through only one.  So a bare `bash: deny` still stops a `Path` head
    /// `/bin/bash` that a covering `/bin/` allow dir would otherwise admit,
    /// while a planted `/tmp/evil/rg` still cannot inherit a bare `rg: allow`
    /// — the basename lands here and never in `policy_names`.  It takes the
    /// narrow set rather than rebuilding it because both callers,
    /// `capability::admits_head` and `super::vet`, need both, and the PATH
    /// walk should be paid once.
    pub(crate) fn deny_names_from(&self, mut names: Vec<String>) -> Vec<String> {
        for base in [
            crate::path::basename(&self.resolved),
            crate::path::basename(&self.shown),
        ] {
            if !names.iter().any(|n| n == base) {
                names.push(base.to_string());
            }
        }
        names
    }
}

/// Surface rendering of `name`: bare and path heads verbatim, tilde heads
/// expanded against the effective `HOME`.  A `~user` head off Unix, where
/// there is no `getpwnam(3)`, falls back to its literal spelling — keeping
/// resolution total, and leaving it to fail downstream as an ordinary
/// missing command.
fn render(name: &CommandName, ctx: &Context) -> String {
    match name {
        CommandName::Bare(name) => name.clone(),
        CommandName::Path(path) => path.clone(),
        CommandName::TildePath(path) => {
            let home = ctx.home();
            expand_tilde_path(path.user.as_deref(), path.suffix.as_deref(), &home)
                .unwrap_or_else(|| path.to_literal())
        }
    }
}

/// PATH walk for bare names against ral's effective `PATH`.
///
/// A `within [env: [PATH: …]]` override rewrites the search list for the
/// block, so resolving here and handing `Command::new` an absolute path
/// keeps ral's view authoritative over `posix_spawnp(3)`'s own parent-PATH
/// search.  A miss falls through as input, leaving the OS to produce its
/// "not found" at spawn time.
fn walk_path(name: &CommandName, ctx: &Context) -> String {
    let rendered = render(name, ctx);
    if let CommandName::Bare(_) = name {
        let path = ctx.env_overrides().get_or_host("PATH").unwrap_or_default();
        if let Some(resolved) = crate::path::resolve_in_path(&rendered, &path, ctx.dir.as_deref()) {
            return resolved;
        }
    }
    rendered
}

/// Lexically resolve a relative path against ral's effective cwd, folding
/// `.` and `..`.  `None` when `s` is already absolute — the caller needs no
/// second candidate then.
fn absolutize(s: &str, ctx: &Context) -> Option<String> {
    if crate::path::is_absolute(s) {
        return None;
    }
    let cwd = ctx.dir.as_deref();
    Some(
        crate::path::resolve_path(cwd, s)
            .to_string_lossy()
            .into_owned(),
    )
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::types::Shell;
    #[cfg(unix)]
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn render_expands_tilde_against_env_home() {
        let mut shell = Shell::default();
        shell.mobile.context.set_env_var("HOME", "/tmp/home");
        assert_eq!(
            render(
                &CommandName::TildePath(crate::path::tilde::TildePath {
                    user: None,
                    suffix: Some("/.local/bin/claude".into()),
                }),
                &shell.mobile.context,
            ),
            "/tmp/home/.local/bin/claude",
        );
    }

    #[test]
    fn render_returns_bare_name_verbatim() {
        let shell = Shell::default();
        assert_eq!(
            render(
                &CommandName::Bare("/usr/local/bin/claude".into()),
                &shell.mobile.context,
            ),
            "/usr/local/bin/claude",
        );
    }

    #[test]
    fn render_returns_path_head_verbatim() {
        let shell = Shell::default();
        assert_eq!(
            render(
                &CommandName::Path("./bin/claude".into()),
                &shell.mobile.context,
            ),
            "./bin/claude",
        );
    }

    /// Unix-only: the expected candidate is spelled with POSIX separators.
    #[cfg(unix)]
    #[test]
    fn policy_names_surface_cwd_absolute_for_relative_path_head() {
        let mut shell = Shell::default();
        shell.mobile.context.dir = Some("/tmp/jq_src/jq-1.7".into());
        let names = CommandIdentity::resolve(
            CommandName::Path("./configure".into()),
            &shell.mobile.context,
        )
        .policy_names(&shell.mobile.context);
        assert_eq!(
            names,
            vec![
                "./configure".to_string(),
                "/tmp/jq_src/jq-1.7/configure".to_string(),
            ],
        );
    }

    #[cfg(unix)]
    #[test]
    fn policy_names_do_not_duplicate_absolute_path_head() {
        let shell = Shell::default();
        let names = CommandIdentity::resolve(
            CommandName::Path("/usr/local/bin/configure".into()),
            &shell.mobile.context,
        )
        .policy_names(&shell.mobile.context);
        assert_eq!(names, vec!["/usr/local/bin/configure".to_string()]);
    }

    /// A policy that denies `bash` by bare name yet allows all of `/bin/`.
    /// Feeding the narrow set as both identities admits `/bin/bash` through
    /// the allow dir; the broad `deny_names` vetoes it.
    #[cfg(unix)]
    #[test]
    fn deny_names_close_path_bash_bypass_of_bare_deny() {
        use crate::capability::admits_for_test;
        use crate::path::NormalizedPrefix;
        use crate::types::{Capabilities, Context, ExecMap, ExecPolicy, GrantStack};

        let mut grants = GrantStack::root();
        grants.push(Capabilities {
            exec: Some(ExecMap {
                literals: BTreeMap::from([("bash".into(), ExecPolicy::Deny)]),
                allow_dirs: BTreeSet::from([NormalizedPrefix::from_surface("/bin")]),
                deny_dirs: BTreeSet::new(),
            }),
            ..Capabilities::root()
        });
        let ctx = Context {
            grants,
            ..Context::default()
        };
        let id = CommandIdentity::resolve(CommandName::Path("/bin/bash".into()), &ctx);
        let allow = id.policy_names(&ctx);
        let allow_refs: Vec<&str> = allow.iter().map(String::as_str).collect();
        let deny = id.deny_names_from(id.policy_names(&ctx));
        let deny_refs: Vec<&str> = deny.iter().map(String::as_str).collect();

        // Narrow set fed as both identities: the hole is open.
        assert!(
            admits_for_test(&ctx.grants, &allow_refs, &allow_refs),
            "narrow-only gate admits /bin/bash (the pre-fix bypass)",
        );
        assert!(
            !allow.iter().any(|n| n == "bash"),
            "policy_names must NOT carry the bare basename for a Path head",
        );
        assert!(
            deny.iter().any(|n| n == "bash"),
            "deny_names must surface the bare basename for a Path head",
        );
        // The broad deny set closes it.
        assert!(
            !admits_for_test(&ctx.grants, &deny_refs, &allow_refs),
            "broad deny_names veto closes the /bin/bash bypass",
        );
    }

    /// The mirror of the bypass test: widening the veto set must not widen
    /// admission, so a bare `rg: allow` cannot reach a planted `/tmp/evil/rg`.
    #[cfg(unix)]
    #[test]
    fn deny_names_basename_does_not_admit_planted_path_head() {
        use crate::capability::admits_for_test;
        use crate::types::{Capabilities, Context, ExecMap, ExecPolicy, GrantStack};

        let mut grants = GrantStack::root();
        grants.push(Capabilities {
            exec: Some(ExecMap {
                literals: BTreeMap::from([("rg".into(), ExecPolicy::Allow)]),
                allow_dirs: BTreeSet::new(),
                deny_dirs: BTreeSet::new(),
            }),
            ..Capabilities::root()
        });
        let ctx = Context {
            grants,
            ..Context::default()
        };
        let id = CommandIdentity::resolve(CommandName::Path("/tmp/evil/rg".into()), &ctx);
        let allow = id.policy_names(&ctx);
        let allow_refs: Vec<&str> = allow.iter().map(String::as_str).collect();
        let deny = id.deny_names_from(id.policy_names(&ctx));
        let deny_refs: Vec<&str> = deny.iter().map(String::as_str).collect();

        assert!(
            !allow.iter().any(|n| n == "rg"),
            "policy_names must not carry the basename for a planted Path head",
        );
        assert!(
            deny.iter().any(|n| n == "rg"),
            "deny_names carries the basename (harmless: it is an allow, not a deny)",
        );
        assert!(
            !admits_for_test(&ctx.grants, &deny_refs, &allow_refs),
            "bare rg: allow must not admit a Path-invoked /tmp/evil/rg",
        );
    }

    #[cfg(unix)]
    #[test]
    fn policy_names_drop_bare_when_scoped_path_diverges() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("git");
        std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&tool).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tool, perms).unwrap();

        let mut shell = Shell::default();
        shell
            .mobile
            .context
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned());

        let id = CommandIdentity::resolve(CommandName::Bare("git".into()), &shell.mobile.context);
        let names = id.policy_names(&shell.mobile.context);
        assert_eq!(names, vec![id.resolved]);
    }
}
