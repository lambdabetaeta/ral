//! What command did the user name, and what will run?
//!
//! [`Head`] freezes the answer at dispatch — the spelling we show, and the
//! [`Program`] a grant judges and the launcher runs — and is threaded down to
//! launch, so classification, guard and spawn see one program and `PATH` is
//! walked once.  A head that names nothing runnable has no program: its
//! [`Missing`] is the 127 or 126 `super::vet` reports, read off this walk
//! rather than re-probed.

use crate::capability::Program;
use crate::ir::CommandName;
use crate::path::{PathSearch, RealPath, is_executable_file};
use crate::process::SpawnFailure;
use crate::types::{Context, Error};
use std::io::ErrorKind;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub(crate) struct Head {
    pub(crate) shown: String,
    pub(crate) program: Result<Program, Missing>,
}

/// Why a head has no program.
#[derive(Clone, Debug)]
pub(crate) enum Missing {
    NotFound,
    /// The file named or found, which cannot be run.
    NotExecutable(PathBuf),
}

impl Head {
    /// Render `name` against `ctx`, and find what it runs: a bundled tool by
    /// its name alone, so the in-binary implementation wins over a host
    /// twin; any other bare name by the effective `PATH`; a path where the
    /// launcher's child will find it.
    pub(crate) fn resolve(name: &CommandName, ctx: &Context) -> Self {
        let shown = render(name, ctx);
        let program = match name {
            CommandName::Bare(bare) if crate::uutils::is_uutils_tool(bare) => {
                Ok(Program::Tool(bare.to_string()))
            }
            CommandName::Bare(_) => {
                let path = ctx.env_overrides().get_or_host("PATH");
                match crate::path::search(&shown, path.as_deref(), ctx.search_cwd()) {
                    PathSearch::Executable(hit) => Program::file(hit),
                    PathSearch::FoundNotExecutable(found) => Err(Missing::NotExecutable(found)),
                    PathSearch::Missing => Err(Missing::NotFound),
                }
            }
            CommandName::Path(_) | CommandName::TildePath(_) => {
                Program::file(launch_path(&shown, ctx))
            }
        };
        Self { shown, program }
    }
}

impl Program {
    /// The file `path` names, which the launcher will run by its real path,
    /// seen as `path` by the program.
    pub(crate) fn file(path: PathBuf) -> Result<Self, Missing> {
        let real = RealPath::of(&path).map_err(|e| match e.kind() {
            ErrorKind::NotFound | ErrorKind::NotADirectory => Missing::NotFound,
            _ => Missing::NotExecutable(path.clone()),
        })?;
        if !is_executable_file(&path) {
            return Err(Missing::NotExecutable(path));
        }
        Ok(Self::File { path, real })
    }
}

impl Missing {
    /// The refusal for a head named `shown` that has no program.
    pub(crate) fn error(&self, shown: &str, ctx: &Context) -> Error {
        match self {
            Self::NotFound => {
                let err = Error::spawn_failure(shown, SpawnFailure::NotFound);
                match suffixed(shown, ctx) {
                    Some(meant) => err.with_hint(format!("did you mean '{meant}'?")),
                    None => err,
                }
            }
            // The walk kept the file it stopped at, so the refusal can name
            // it: "permission denied" alone leaves the user guessing which of
            // several `PATH` directories shadowed the one they meant.
            Self::NotExecutable(found) => Error::spawn_failure(
                shown,
                SpawnFailure::PermissionDenied {
                    found: Some(found.as_path().into()),
                },
            ),
        }
    }
}

/// On Windows, a path head written without its extension: `shown` with the
/// first executable extension that names a file.  A bare name has already
/// tried them all on `PATH`.
fn suffixed(shown: &str, ctx: &Context) -> Option<String> {
    if !cfg!(windows) || !shown.contains(['/', '\\']) {
        return None;
    }
    crate::path::WINDOWS_EXEC_EXTENSIONS
        .iter()
        .map(|ext| format!("{shown}.{ext}"))
        .find(|candidate| ctx.resolver().resolve(candidate).as_path().exists())
}

/// Where the launcher's child finds `shown`: anchored at the cwd it starts
/// in.  Off Windows spelled as the user wrote it, the kernel walking it: a
/// fold would judge a different file than a `..` after a link, or a trailing
/// `/`, reaches.  Win32 folds `..` lexically itself, and an absolute path
/// keeps `CreateProcessW` off the process cwd.
fn launch_path(shown: &str, ctx: &Context) -> PathBuf {
    if cfg!(windows) {
        return ctx.resolver().resolve(shown).into_inner();
    }
    ctx.launch_cwd().join(shown)
}

/// Surface rendering of `name`; an unexpandable `~` falls back to its
/// spelling and fails downstream as a missing command.
fn render(name: &CommandName, ctx: &Context) -> String {
    match name {
        CommandName::Bare(name) => name.to_string(),
        CommandName::Path(path) => path.clone(),
        CommandName::TildePath(path) => ctx
            .home()
            .map_or_else(|| path.to_literal(), |home| path.expand(&home)),
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::capability::admits_head;
    use crate::path::FrozenPath;
    use crate::types::{Capabilities, ExecGrant, ExecKey, GrantStack, Shell, Verdict};
    use std::path::Path;

    #[test]
    fn render_expands_tilde_against_env_home() {
        let mut shell = Shell::default();
        shell.context.set_env_var("HOME", "/tmp/home");
        assert_eq!(
            render(
                &CommandName::TildePath(crate::path::tilde::TildePath {
                    suffix: Some("/.local/bin/claude".into()),
                }),
                &shell.context,
            ),
            "/tmp/home/.local/bin/claude",
        );
    }

    /// A context whose sole grant layer is `exec`.
    fn under(exec: ExecGrant) -> Context {
        let mut grants = GrantStack::root();
        grants.push(Capabilities {
            exec: Some(exec),
            ..Capabilities::root()
        });
        let mut ctx = Context::default();
        ctx.grants = grants;
        ctx
    }

    fn path_head(file: &Path) -> CommandName {
        CommandName::Path(file.to_string_lossy().into_owned())
    }

    fn file_of(head: &Head) -> (&Path, &RealPath) {
        match &head.program {
            Ok(Program::File { path, real }) => (path, real),
            other => panic!("expected a host file, got {other:?}"),
        }
    }

    fn canonical(path: &Path) -> RealPath {
        RealPath::of(path).expect("the file exists")
    }

    /// A path head runs the file it names from the shell's cwd, and is
    /// judged by that file's real path.
    #[test]
    fn a_relative_path_head_is_the_file_under_the_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "configure");
        let mut shell = Shell::default();
        shell.seed_cwd(tmp.path().to_path_buf());

        let head = Head::resolve(&CommandName::Path(format!("./{name}")), &shell.context);
        let (path, real) = file_of(&head);
        let file = canonical(&tmp.path().join(&name));
        assert_eq!(real, &file);
        assert_eq!(canonical(path), file);
    }

    /// `link/../tool` runs `real/tool` when `link` points at `real/sub`: the
    /// kernel walks the `..` after the link, so the launcher is handed the
    /// spelling unfolded and the guard judges what it reaches.
    #[cfg(unix)]
    #[test]
    fn a_dotdot_after_a_link_is_judged_where_the_kernel_lands() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        plant(&real, "tool");
        std::os::unix::fs::symlink(real.join("sub"), tmp.path().join("link")).unwrap();
        let mut shell = Shell::default();
        shell.seed_cwd(tmp.path().to_path_buf());

        let head = Head::resolve(&CommandName::Path("link/../tool".into()), &shell.context);
        let (path, judged) = file_of(&head);
        assert_eq!(judged, &canonical(&real.join("tool")));
        assert_eq!(path, tmp.path().join("link/../tool"));
    }

    #[test]
    fn a_path_head_naming_no_file_has_no_program() {
        let shell = Shell::default();
        let head = std::env::temp_dir().join("no-such-dir").join("configure");
        let head = Head::resolve(&path_head(&head), &shell.context);
        assert!(matches!(head.program, Err(Missing::NotFound)));
    }

    /// The kernel refuses `file/` with ENOTDIR, so a trailing slash must not
    /// be folded away into a runnable spelling.
    #[cfg(unix)]
    #[test]
    fn a_trailing_slash_on_a_file_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "tool");
        let shell = Shell::default();
        let head = tmp.path().join(name).to_string_lossy().into_owned() + "/";
        let head = Head::resolve(&CommandName::Path(head), &shell.context);
        assert!(matches!(head.program, Err(Missing::NotFound)));
    }

    #[cfg(unix)]
    #[test]
    fn a_path_head_that_cannot_be_run_is_not_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("plain");
        std::fs::write(&file, "").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let shell = Shell::default();
        for head in [file.as_path(), tmp.path()] {
            let head = Head::resolve(&path_head(head), &shell.context);
            assert!(
                matches!(head.program, Err(Missing::NotExecutable(_))),
                "{head:?}"
            );
        }
    }

    /// There is nothing to judge in an absent file: classification lets the
    /// head through, and `vet` names the missing command.
    #[test]
    fn a_head_with_no_program_is_admitted_under_any_grant() {
        let ctx = under(ExecGrant::default());
        let head = std::env::temp_dir().join("no-such-dir").join("tool");
        let head = Head::resolve(&path_head(&head), &ctx);
        assert!(admits_head(&ctx, &head));
    }

    /// A bare name runs what the walk found, never a same-named file in the
    /// cwd.
    #[test]
    fn a_bare_name_missing_from_path_is_not_found_beside_a_cwd_file() {
        crate::path::forget_located_commands();
        let here = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let name = plant(here.path(), "zzcwdfile");

        let mut shell = Shell::default();
        shell.seed_cwd(here.path().to_path_buf());
        shell
            .context
            .set_env_var("PATH", elsewhere.path().to_string_lossy().into_owned());

        let head = Head::resolve(&CommandName::Bare(name.into()), &shell.context);
        assert!(matches!(head.program, Err(Missing::NotFound)));
    }

    /// The 126 case, which only a platform with an executable bit has.
    #[cfg(unix)]
    #[test]
    fn a_non_executable_path_hit_is_not_executable() {
        use std::os::unix::fs::PermissionsExt;
        crate::path::forget_located_commands();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("zznoexec");
        std::fs::write(&file, "").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut shell = Shell::default();
        shell
            .context
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned());

        let head = Head::resolve(&CommandName::Bare("zznoexec".into()), &shell.context);
        assert!(matches!(head.program, Err(Missing::NotExecutable(found)) if found == file));
    }

    /// A bundled name is its tool whatever `PATH` holds: no walk is taken.
    #[cfg(feature = "coreutils")]
    #[test]
    fn a_bundled_name_is_its_tool() {
        let dir = tempfile::tempdir().unwrap();
        let mut shell = Shell::default();
        shell
            .context
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned());
        let head = Head::resolve(&CommandName::Bare("ls".into()), &shell.context);
        assert!(matches!(head.program, Ok(Program::Tool(tool)) if tool == "ls"));
    }

    /// A bare key is the file the host `PATH` finds, so a scoped `PATH` that
    /// plants another `git` does not inherit `git: allow`.
    #[test]
    fn a_scoped_path_cannot_redirect_a_bare_key() {
        crate::path::forget_located_commands();
        let dir = tempfile::tempdir().unwrap();
        let name = plant(dir.path(), "git");
        let mut ctx =
            under(std::iter::once((ExecKey::Name("git".into()), Verdict::Allow)).collect());
        ctx.set_env_var("PATH", dir.path().to_string_lossy().into_owned());

        let head = Head::resolve(&CommandName::Bare("git".into()), &ctx);
        assert_eq!(file_of(&head).1, &canonical(&dir.path().join(name)));
        assert!(!admits_head(&ctx, &head));
    }

    /// `bin/bash` real, `planted/b` a symlink to it: the shape the evasion
    /// takes.  Returns both directories and the link.
    #[cfg(unix)]
    fn planted_symlink(tmp: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let bin = tmp.join("bin");
        let planted = tmp.join("planted");
        std::fs::create_dir(&bin).unwrap();
        std::fs::create_dir(&planted).unwrap();
        plant(&bin, "bash");
        let link = planted.join("b");
        std::os::unix::fs::symlink(bin.join("bash"), &link).unwrap();
        (bin, planted, link)
    }

    fn dir_allow(dir: &Path) -> (ExecKey, Verdict) {
        (ExecKey::Dir(FrozenPath::from_surface(dir)), Verdict::Allow)
    }

    /// A policy that denies `bash` by bare name yet allows all of its bin
    /// dir: the file that runs is `bash` whatever the head spelled, so the
    /// veto beats the dir.
    #[test]
    fn a_bare_deny_beats_the_allow_dir_a_path_head_sits_in() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "bash");
        let ctx = under(
            [
                (ExecKey::Name("bash".into()), Verdict::Deny),
                dir_allow(tmp.path()),
            ]
            .into_iter()
            .collect(),
        );

        let head = Head::resolve(&path_head(&tmp.path().join(name)), &ctx);
        assert!(!admits_head(&ctx, &head));
    }

    /// The mirror: a bare `rg: allow` is the host's `rg`, so a planted path
    /// head `evil/rg` does not inherit it.
    #[test]
    fn a_bare_allow_does_not_admit_a_planted_path_head() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "rg");
        let ctx = under(std::iter::once((ExecKey::Name("rg".into()), Verdict::Allow)).collect());

        let head = Head::resolve(&path_head(&tmp.path().join(name)), &ctx);
        assert!(!admits_head(&ctx, &head));
    }

    /// The symlink half of the deny-by-name evasion: `planted/b` wears a name
    /// under the allow dir and runs a file the grant denies.  The program is
    /// that file, so the link is denied by its name.
    #[cfg(unix)]
    #[test]
    fn a_symlink_under_an_allow_dir_meets_the_bare_deny_of_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin, planted, link) = planted_symlink(tmp.path());
        let ctx = under(
            [
                (ExecKey::Name("bash".into()), Verdict::Deny),
                dir_allow(&planted),
            ]
            .into_iter()
            .collect(),
        );

        let head = Head::resolve(&path_head(&link), &ctx);
        assert_eq!(file_of(&head).1, &canonical(&bin.join("bash")));
        assert!(!admits_head(&ctx, &head));
    }

    /// The same link, judged by a denied *region* its program lies in.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_deny_dir_is_denied_by_the_file_it_names() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin, planted, link) = planted_symlink(tmp.path());
        let ctx = under(
            [
                dir_allow(&planted),
                (ExecKey::Dir(FrozenPath::from_surface(&bin)), Verdict::Deny),
            ]
            .into_iter()
            .collect(),
        );

        let head = Head::resolve(&path_head(&link), &ctx);
        assert!(!admits_head(&ctx, &head));
    }

    /// A path key names a file, so a link to it runs the admitted file.
    #[cfg(unix)]
    #[test]
    fn a_symlink_is_admitted_by_the_path_key_on_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin, _planted, link) = planted_symlink(tmp.path());
        let ctx = under(
            std::iter::once((
                ExecKey::Path(FrozenPath::from_surface(bin.join("bash"))),
                Verdict::Allow,
            ))
            .collect(),
        );

        let head = Head::resolve(&path_head(&link), &ctx);
        assert!(admits_head(&ctx, &head));
    }

    /// Where the scheme stops, pinned rather than left to be rediscovered: a
    /// *copy* is a different file, holding no trace of the name that was
    /// denied, and an allow dir admits it.  No resolution can close this, and
    /// nothing needs to: the copy runs under the projection that admitted the
    /// write, so it reaches nothing its author could not.
    #[test]
    fn a_copy_under_a_new_name_is_a_different_file_and_is_admitted() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        let planted = tmp.path().join("planted");
        std::fs::create_dir(&bin).unwrap();
        std::fs::create_dir(&planted).unwrap();
        let name = plant(&bin, "bash");
        let copy = planted.join("b");
        std::fs::copy(bin.join(&name), &copy).unwrap();

        let ctx = under(
            [
                (ExecKey::Name("bash".into()), Verdict::Deny),
                dir_allow(&planted),
            ]
            .into_iter()
            .collect(),
        );

        let head = Head::resolve(&path_head(&copy), &ctx);
        assert!(admits_head(&ctx, &head));
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

    /// A `./bin` on `PATH` follows the shell's cwd, the one "here" every
    /// other consumer reads.
    #[test]
    fn walk_anchors_relative_path_entries_to_the_cwd() {
        crate::path::forget_located_commands();
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let name = plant(&bin, "zzwalk");

        let mut shell = Shell::default();
        shell.seed_cwd(tmp.path().to_path_buf());
        shell.context.set_env_var("PATH", "./bin");

        let head = Head::resolve(&CommandName::Bare(name.as_str().into()), &shell.context);
        assert_eq!(file_of(&head).0, bin.join(&name));
    }
}
