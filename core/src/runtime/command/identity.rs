//! What command did the user name?
//!
//! [`CommandIdentity`] freezes the answer in three fields: the
//! `CommandName` the user wrote, the `shown` string we display, and the
//! `resolved` absolute path (or the bare name when PATH lookup
//! misses).  Built once at dispatch classification and threaded down
//! to launch, so classify and exec agree on which executable they
//! are talking about — no second PATH walk, no TOCTOU window.
//!
//! Identity is total: a missing bare name keeps `resolved == shown`
//! and the 127/126 decision is deferred to [`super::vet`].

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
    /// Render and PATH-resolve `name` against `ctx`.  Never fails: a
    /// bare name that misses on PATH keeps `resolved == shown`.
    pub(crate) fn resolve(name: CommandName, ctx: &Context) -> Self {
        let shown = render(&name, ctx);
        let resolved = walk_path(&name, ctx);
        Self {
            name,
            shown,
            resolved,
        }
    }

    /// Candidate names by which this head matches an `exec`
    /// capability key, in admission-order.
    ///
    /// `Bare` heads drop the surface name from the candidate list
    /// whenever the active scope's `PATH` redirects resolution away
    /// from the host `PATH`: an outer grant keyed by the bare name
    /// must not silently admit a spoofed binary that only exists on
    /// a temporary search path.  `Path` heads pick up the cwd-joined
    /// absolute form so a directory-keyed grant covering the working
    /// tree can admit them — `exec_dirs` matchers require absolute
    /// paths.
    pub(crate) fn policy_names(&self, ctx: &Context) -> Vec<String> {
        let mut names = Vec::new();
        let mut include_rendered = true;
        if matches!(self.name, CommandName::Bare(_)) {
            let baseline = std::env::var("PATH")
                .ok()
                .and_then(|path| crate::path::resolve_in_path(&self.shown, &path));
            if baseline.as_deref() != Some(self.resolved.as_str()) && self.resolved != self.shown {
                include_rendered = false;
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
}

/// Surface rendering of `name`: bare and path heads are returned
/// verbatim; tilde heads expand against the effective `HOME`.
fn render(name: &CommandName, ctx: &Context) -> String {
    match name {
        CommandName::Bare(name) => name.clone(),
        CommandName::Path(path) => path.clone(),
        CommandName::TildePath(path) => {
            let home = ctx.home();
            expand_tilde_path(path.user.as_deref(), path.suffix.as_deref(), &home)
        }
    }
}

/// PATH walk for bare names against ral's effective `PATH`.
///
/// A `within [shell: [PATH: …]]` override changes the search list for
/// every command inside the block; resolving here and handing the
/// absolute path to `Command::new` keeps ral's view authoritative
/// over `posix_spawnp(3)`'s parent-PATH search.  A name already
/// containing `/` is a path and returned unchanged; a bare name that
/// misses on PATH falls through as input so the OS produces its
/// normal "not found" error at spawn time.
fn walk_path(name: &CommandName, ctx: &Context) -> String {
    let rendered = render(name, ctx);
    if let CommandName::Bare(_) = name {
        let path = ctx.env_overrides().get_or_host("PATH").unwrap_or_default();
        if let Some(resolved) = crate::path::resolve_in_path(&rendered, &path) {
            return resolved;
        }
    }
    rendered
}

/// Lexically resolve a relative path against ral's effective cwd,
/// collapsing `.` and `..`.  Returns `None` when `s` is already
/// absolute — the caller treats that as "no need to add another
/// candidate."
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

    /// `./configure` is denied by `exec_dirs` (which require absolute
    /// paths) unless `policy_names` also surfaces the cwd-joined form.
    /// Unix-only: on Windows the path resolver produces a `C:\…`
    /// duplicate that the no-dup invariant rejects, and the grant
    /// subsystem this serves is Unix-only anyway.
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

    /// Already-absolute path heads emit exactly one candidate — no
    /// duplicate from `absolutize`.
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

    /// When a scoped `PATH` redirects resolution away from the host
    /// PATH, the bare surface name drops out of the candidate list so
    /// an outer grant keyed on the bare name cannot silently admit
    /// the spoofed binary.
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
