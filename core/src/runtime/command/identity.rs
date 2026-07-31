//! What command did the user name?
//!
//! [`CommandIdentity`] freezes the answer at dispatch — the surface
//! spelling we show and the PATH-walked form we exec — and is threaded
//! down to launch, so classification and spawn cannot disagree and PATH
//! is walked once.  Resolution is total: a bare name that misses on PATH
//! keeps `resolved == shown`.
//!
//! The module invariant: **`resolved` and `search` are two projections of one
//! walk, and `super::vet` may not walk again.**  The 126/127 verdict is a
//! pattern match on the [`PathSearch`] this walk recorded, not a second probe
//! that could answer against another anchor.

use crate::ir::CommandName;
use crate::path::{PathSearch, tilde::expand_tilde_path};
use crate::types::Context;

#[derive(Clone, Debug)]
pub(crate) struct CommandIdentity {
    pub(crate) name: CommandName,
    pub(crate) shown: String,
    pub(crate) resolved: String,
    /// What the walk that produced `resolved` saw, for a `Bare` head; `None`
    /// for a path or tilde head, which `PATH` never searched and whose absence
    /// the kernel reports at spawn.
    pub(crate) search: Option<PathSearch>,
}

impl CommandIdentity {
    /// Render and PATH-resolve `name` against `ctx`.
    pub(crate) fn resolve(name: CommandName, ctx: &Context) -> Self {
        let shown = render(&name, ctx);
        let (resolved, search) = walk_path(&name, ctx);
        Self {
            name,
            shown,
            resolved,
            search,
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
    ///
    /// The baseline walk asks a genuinely different question — the *host*
    /// `PATH` rather than the effective one — so it is its own traversal; but
    /// it anchors where [`walk_path`] anchors, `Context::search_cwd`.  A
    /// baseline taken against a different "here" compares two resolutions of
    /// two commands, and the grant gate would then judge an identity vet never
    /// saw.
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
                    crate::path::resolve_in_path(&self.shown, path, ctx.search_cwd())
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

/// PATH walk for bare names against ral's effective `PATH`, returning both of
/// the walk's projections: the form to exec, and what the walk saw.
///
/// A `within [env: [PATH: …]]` override rewrites the search list for the
/// block, so resolving here and handing `Command::new` an absolute path
/// keeps ral's view authoritative over `posix_spawnp(3)`'s own parent-PATH
/// search.  A miss falls through as input, leaving the OS to produce its
/// "not found" at spawn time — and leaving `super::vet` a verdict from *this*
/// traversal to read rather than a probe of its own to take.
///
/// The verdict's presence half costs one stat per `PATH` entry, and only when
/// the executable walk missed: the error path, and bare names of bundled tools
/// whose host `PATH` holds no twin.
fn walk_path(name: &CommandName, ctx: &Context) -> (String, Option<PathSearch>) {
    let rendered = render(name, ctx);
    let CommandName::Bare(_) = name else {
        return (rendered, None);
    };
    let path = ctx.env_overrides().get_or_host("PATH").unwrap_or_default();
    let search = crate::path::search(&rendered, Some(&path), ctx.search_cwd());
    let resolved = match &search {
        PathSearch::Executable(hit) => hit.to_string_lossy().into_owned(),
        PathSearch::FoundNotExecutable(_) | PathSearch::Missing => rendered,
    };
    (resolved, Some(search))
}

/// Lexically resolve a relative path against ral's effective cwd, folding
/// `.` and `..`.  `None` when `s` is already absolute — the caller needs no
/// second candidate then.
///
/// "Effective" is `Context::cwd_chain`, the same precedence the walk anchors
/// to: a policy name absolutised against `self.dir` alone would name a
/// different file than the one a `cd`'d shell is about to spawn.
fn absolutize(s: &str, ctx: &Context) -> Option<String> {
    if crate::path::is_absolute(s) {
        return None;
    }
    let cwd = ctx.cwd_chain();
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
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

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

    #[test]
    fn policy_names_surface_cwd_absolute_for_relative_path_head() {
        let dir = std::env::temp_dir().join("jq_src").join("jq-1.7");
        let mut shell = Shell::default();
        let names = shell.with_cwd(dir.clone(), |shell| {
            CommandIdentity::resolve(
                CommandName::Path("./configure".into()),
                &shell.mobile.context,
            )
            .policy_names(&shell.mobile.context)
        });
        assert_eq!(
            names,
            vec![
                "./configure".to_string(),
                dir.join("configure").to_string_lossy().into_owned(),
            ],
        );
    }

    #[test]
    fn policy_names_do_not_duplicate_absolute_path_head() {
        let head = std::env::temp_dir()
            .join("bin")
            .join("configure")
            .to_string_lossy()
            .into_owned();
        let shell = Shell::default();
        let names =
            CommandIdentity::resolve(CommandName::Path(head.clone()), &shell.mobile.context)
                .policy_names(&shell.mobile.context);
        assert_eq!(names, vec![head]);
    }

    /// A policy that denies `bash` by bare name yet allows all of its bin
    /// dir.  Feeding the narrow set as both identities admits the bypass
    /// through the allow dir; the broad `deny_names` vetoes it.
    #[test]
    fn deny_names_close_path_bash_bypass_of_bare_deny() {
        use crate::capability::admits_for_test;
        use crate::path::NormalizedPrefix;
        use crate::types::{Capabilities, Context, ExecMap, ExecPolicy, GrantStack};

        let bin = std::env::temp_dir().join("bin");
        let bash = bin.join("bash");
        let mut grants = GrantStack::root();
        grants.push(Capabilities {
            exec: Some(ExecMap {
                literals: BTreeMap::from([("bash".into(), ExecPolicy::Deny)]),
                allow_dirs: BTreeSet::from([NormalizedPrefix::from_surface(&bin)]),
                deny_dirs: BTreeSet::new(),
            }),
            ..Capabilities::root()
        });
        let mut ctx = Context::default();
        ctx.grants = grants;
        let id =
            CommandIdentity::resolve(CommandName::Path(bash.to_string_lossy().into_owned()), &ctx);
        let allow = id.policy_names(&ctx);
        let allow_refs: Vec<&str> = allow.iter().map(String::as_str).collect();
        let deny = id.deny_names_from(id.policy_names(&ctx));
        let deny_refs: Vec<&str> = deny.iter().map(String::as_str).collect();

        // Narrow set fed as both identities: the hole is open.
        assert!(
            admits_for_test(&ctx.grants, &allow_refs, &allow_refs),
            "narrow-only gate admits the planted bash (the bypass)",
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
            "broad deny_names veto closes the bypass",
        );
    }

    /// The mirror of the bypass test: widening the veto set must not widen
    /// admission, so a bare `rg: allow` cannot reach a planted head elsewhere.
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
        let mut ctx = Context::default();
        ctx.grants = grants;
        let head = std::env::temp_dir().join("evil").join("rg");
        let id =
            CommandIdentity::resolve(CommandName::Path(head.to_string_lossy().into_owned()), &ctx);
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
            "bare rg: allow must not admit a Path-invoked planted rg",
        );
    }

    /// An executable a *bare-name* walk finds in `dir`: off Unix a bare name
    /// only resolves through `%PATHEXT%`, so the file needs a suffix from it.
    fn plant(dir: &Path, stem: &str) -> String {
        let name = if cfg!(windows) {
            format!("{stem}.bat")
        } else {
            stem.to_owned()
        };
        std::fs::write(dir.join(&name), b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.join(&name);
            let mut perms = std::fs::metadata(&p).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&p, perms).unwrap();
        }
        name
    }

    /// A `./bin` on `PATH` must follow the shell, and in a plain REPL "the
    /// shell's here" is `cwd.current` — `ctx.dir` is bound only inside
    /// `within [dir: …]`, so a walk reading it alone anchors to nothing.
    #[test]
    fn walk_anchors_relative_path_entries_to_the_cd_cwd() {
        crate::path::forget_located_commands();
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let name = plant(&bin, "zzwalk");

        let mut shell = Shell::default();
        shell.seed_cwd(tmp.path().to_path_buf());
        shell.mobile.context.set_env_var("PATH", "./bin");

        let id = CommandIdentity::resolve(CommandName::Bare(name.clone()), &shell.mobile.context);
        assert_eq!(id.resolved, bin.join(&name).to_string_lossy());
    }

    /// The `within [dir: …]` override outranks the `cd`-mutated cwd, and the
    /// walk must read the same precedence every other consumer of "here" does.
    #[test]
    fn dir_override_outranks_the_cd_cwd_for_the_walk() {
        crate::path::forget_located_commands();
        let overridden = tempfile::tempdir().unwrap();
        let cd_to = tempfile::tempdir().unwrap();
        let name = {
            for root in [overridden.path(), cd_to.path()] {
                std::fs::create_dir(root.join("bin")).unwrap();
            }
            plant(&cd_to.path().join("bin"), "zzboth");
            plant(&overridden.path().join("bin"), "zzboth")
        };

        let mut shell = Shell::default();
        shell.seed_cwd(cd_to.path().to_path_buf());
        shell.mobile.context.set_env_var("PATH", "./bin");

        let id = shell.with_cwd(overridden.path().to_path_buf(), |shell| {
            CommandIdentity::resolve(CommandName::Bare(name.clone()), &shell.mobile.context)
        });
        assert_eq!(
            id.resolved,
            overridden.path().join("bin").join(&name).to_string_lossy(),
        );
    }

    /// The `absolutize` half of the same precedence: a relative `Path` head's
    /// policy name is joined to the effective cwd, `cd`-mutated or not.
    #[test]
    fn policy_absolute_uses_the_cd_cwd() {
        let dir = std::env::temp_dir().join("jq_src").join("jq-1.7");
        let mut shell = Shell::default();
        shell.seed_cwd(dir.clone());
        let names = CommandIdentity::resolve(
            CommandName::Path("./configure".into()),
            &shell.mobile.context,
        )
        .policy_names(&shell.mobile.context);
        assert!(
            names.iter().any(|n| Path::new(n) == dir.join("configure")),
            "got {names:?}",
        );
    }

    #[test]
    fn policy_names_drop_bare_when_scoped_path_diverges() {
        let dir = tempfile::tempdir().unwrap();
        let name = plant(dir.path(), "git");

        let mut shell = Shell::default();
        shell
            .mobile
            .context
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned());

        let id = CommandIdentity::resolve(CommandName::Bare(name), &shell.mobile.context);
        let names = id.policy_names(&shell.mobile.context);
        assert_eq!(names, vec![id.resolved]);
    }
}
