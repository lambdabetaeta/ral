//! What command did the user name, and what will run?
//!
//! [`Head`] freezes the answer at dispatch — the spelling we show, and the
//! [`Program`] a grant judges and the launcher runs, found from the head's
//! native target and never from the spelling — and [`Head::admit`] moves the
//! program into the `Admitted`, which `super::vet` plans a launch from, so
//! classification, guard and spawn see one program held once, and `PATH` is
//! walked once.  A head that names nothing runnable has no program: its
//! [`Missing`] is the 127 or 126 `super::vet` reports, read off this walk
//! rather than re-probed.

use crate::authority::policy::{Admitted, Program};
use crate::ir::CommandName;
use crate::path::{LeafMode, LexicalPath, PathSearch, is_executable_file, walk};
use crate::process::{SpawnFailure, Unrunnable};
use crate::shell::runtime::{Context, Error, Mooring, Settled, Shell};
use crate::site::{Silent, site};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct Head<P = Program> {
    /// The command as ral names it, to the guard and in what it reports: a
    /// bare name as written, a `~` head as
    /// [`TildePath::shown`](crate::path::tilde::TildePath::shown) shows it.
    /// A path head is the one shown path not folded by [`crate::path::shown`]:
    /// it echoes the user's own word, separators as typed, since what ral
    /// reports answers the word the user wrote.
    pub(crate) shown: String,
    /// A [`Program`] as resolved; an [`Admitted`] once [`Head::admit`] judged it.
    pub(crate) program: Result<P, Missing>,
}

/// Why a head has no program.
#[derive(Clone, Debug)]
pub(crate) enum Missing {
    /// Nothing at the name: a bare name no `PATH` directory holds, or the
    /// file a path looked for, kept so the refusal can offer the extension
    /// it lacks.
    NotFound(Option<PathBuf>),
    /// The file named or found, which cannot be run.
    NotExecutable(PathBuf),
}

impl Head {
    /// Judge the program under the live stack: a launch's one exec judgment.
    /// A head with no program passes as it is.
    ///
    /// # Errors
    /// The grant's refusal, recorded against `mooring`.
    pub(crate) fn admit(self, mooring: &Mooring, shell: &mut Shell) -> Settled<Head<Admitted>> {
        let Self { shown, program } = self;
        let program = match program {
            Ok(program) => Ok(shell.check_exec(&shown, program, mooring)?),
            Err(missing) => Err(missing),
        };
        Ok(Head { shown, program })
    }

    /// Render `name` against `ctx`, and find what it runs: a bundled tool by
    /// its name alone, so the in-binary implementation wins over a host
    /// twin; any other bare name by the effective `PATH`; a path where the
    /// launcher's child will find it.
    ///
    /// A `~` head expands natively against `HOME` for its target, and is
    /// shown as `~` shows anywhere else; with no `HOME` it names no file and
    /// is shown as written.
    pub(crate) fn resolve(name: &CommandName, ctx: &Context) -> Self {
        match name {
            CommandName::Bare(bare) => {
                let shown = bare.to_string();
                let program = if let Some(tool) =
                    crate::uutils::bundled_name(std::ffi::OsStr::new(bare.as_ref()))
                {
                    Ok(Program::Tool(tool))
                } else {
                    let path = ctx.env_overrides().get_or_host("PATH");
                    match crate::path::search(&shown, path.as_deref(), Some(ctx.cwd())) {
                        PathSearch::Executable(hit) => Program::file(hit),
                        PathSearch::FoundNotExecutable(found) => Err(Missing::NotExecutable(found)),
                        PathSearch::Missing => Err(Missing::NotFound(None)),
                    }
                };
                Self { shown, program }
            }
            CommandName::Path(path) => Self {
                shown: path.clone(),
                program: Program::file(launch_path(&PathBuf::from(path), ctx)),
            },
            CommandName::TildePath(path) => match ctx.home() {
                Some(home) => Self {
                    shown: path.shown(&home),
                    program: Program::file(launch_path(&path.expand(&home), ctx)),
                },
                None => Self {
                    shown: path.to_literal(),
                    program: Err(Missing::NotFound(None)),
                },
            },
        }
    }
}

impl Program {
    /// The file `path` names, which the launcher will run by its real path,
    /// seen as `path` by the program.  Whether it may be run is not asked
    /// here: a program is judged before it is looked at.
    pub(crate) fn file(path: PathBuf) -> Result<Self, Missing> {
        let missing = || Missing::NotFound(Some(path.clone()));
        let spelled = LexicalPath::absolute(&path).ok_or_else(missing)?;
        let located = walk(&spelled, LeafMode::Follow).map_err(|e| match e.kind() {
            ErrorKind::NotFound | ErrorKind::NotADirectory => missing(),
            _ => Missing::NotExecutable(path.clone()),
        })?;
        if located.trace().kind().is_none() {
            return Err(missing());
        }
        Ok(Self::File {
            arg0: path,
            located: Box::new(located),
        })
    }
}

impl Missing {
    /// The refusal for a head named `shown` that has no program.
    pub(crate) fn error(&self, shown: &str) -> Error {
        match self {
            Self::NotFound(looked_for) => {
                let err = Error::spawn_failure(shown, SpawnFailure::NotFound);
                match looked_for.as_deref().and_then(|file| suffixed(shown, file)) {
                    Some(meant) => err.with_hint(format!("did you mean '{meant}'?")),
                    None => err,
                }
            }
            // The walk kept the file it stopped at, so the refusal can name
            // it: "permission denied" alone leaves the user guessing which of
            // several `PATH` directories shadowed the one they meant.
            Self::NotExecutable(found) => Error::spawn_failure(shown, refusal(shown, found)),
        }
    }
}

/// What stands at `found`, which the walk would not spawn.  A file the stat
/// cannot reach, or one carrying `+x`, is refused by the OS: permission.
fn refusal(shown: &str, found: &Path) -> SpawnFailure {
    let not_a_program = |kind| SpawnFailure::NotAProgram {
        found: (!crate::path::names_a_path(shown)).then(|| found.into()),
        kind,
    };
    if site!(Silent::HeadRefusal, found => is_dir()) {
        not_a_program(Unrunnable::Directory)
    } else if site!(Silent::HeadRefusal, found => exists()) && !is_executable_file(found) {
        not_a_program(Unrunnable::File)
    } else {
        SpawnFailure::PermissionDenied {
            found: Some(found.into()),
        }
    }
}

/// On Windows, a path head written without its extension: `shown` with the
/// first executable extension that names a file beside `file`, the one it
/// looked for.  A bare name has already tried them all on `PATH`.
fn suffixed(shown: &str, file: &Path) -> Option<String> {
    if !cfg!(windows) || !crate::path::names_a_path(shown) {
        return None;
    }
    crate::path::WINDOWS_EXEC_EXTENSIONS
        .iter()
        .find(|ext| {
            site!(
                Silent::HeadSuffixed,
                &crate::path::with_appended_suffix(file, ext) => exists()
            )
        })
        .map(|ext| format!("{shown}.{ext}"))
}

/// Where the launcher's child finds `target`, a path head's native file:
/// anchored at the cwd it starts in, never read for a sigil again.  Off
/// Windows as written, the kernel walking it: a fold would judge a
/// different file than a `..` after a link, or a trailing `/`, reaches.
/// Win32 folds `..` lexically itself, and an absolute path keeps
/// `CreateProcessW` off the process cwd.
fn launch_path(target: &Path, ctx: &Context) -> PathBuf {
    if cfg!(windows) {
        return ctx.resolver().anchor(target).into_inner();
    }
    ctx.launch_cwd().join(target)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::authority::policy::{Baseline, Capabilities, ExecGrant, ExecKey, GrantStack};
    use crate::path::FrozenPath;
    use std::path::Path;

    /// Head admission as dispatch asks it: a head with no program passes.
    fn admits_head(ctx: &Context, head: Head) -> bool {
        (head.program.ok())
            .is_none_or(|program| ctx.grants.admit(&Baseline::default(), program).is_ok())
    }

    #[test]
    fn a_tilde_head_is_shown_expanded_against_env_home() {
        let mut shell = crate::test_helper::core_shell();
        shell.context.set_env_var("HOME", "/tmp/home");
        let head = Head::resolve(
            &CommandName::TildePath(crate::path::tilde::TildePath {
                suffix: Some("/.local/bin/claude".into()),
            }),
            &shell.context,
        );
        assert_eq!(head.shown, "/tmp/home/.local/bin/claude");
    }

    /// A context whose sole grant layer is `exec`.
    fn under(exec: ExecGrant) -> Context {
        let mut grants = GrantStack::root();
        grants.push(crate::test_helper::layer(Capabilities {
            exec: Some(exec),
            ..Capabilities::default()
        }));
        Context::under_grants(grants)
    }

    fn path_head(file: &Path) -> CommandName {
        CommandName::Path(file.to_string_lossy().into_owned())
    }

    fn file_of(head: &Head) -> (&Path, &crate::path::RealPath) {
        match &head.program {
            Ok(Program::File { arg0, located }) => (arg0, located.real()),
            other => panic!("expected a host file, got {other:?}"),
        }
    }

    fn canonical(path: &Path) -> crate::path::RealPath {
        crate::test_helper::trace(path).real().clone()
    }

    /// A path head runs the file it names from the shell's cwd, and is
    /// judged by that file's real path.
    #[test]
    fn a_relative_path_head_is_the_file_under_the_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "configure");
        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(
            crate::path::CwdPath::new(tmp.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );

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
        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(
            crate::path::CwdPath::new(tmp.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );

        let head = Head::resolve(&CommandName::Path("link/../tool".into()), &shell.context);
        let (path, judged) = file_of(&head);
        assert_eq!(judged, &canonical(&real.join("tool")));
        assert_eq!(path, tmp.path().join("link/../tool"));
    }

    #[test]
    fn a_path_head_naming_no_file_has_no_program() {
        let shell = crate::test_helper::core_shell();
        let head = std::env::temp_dir().join("no-such-dir").join("configure");
        let head = Head::resolve(&path_head(&head), &shell.context);
        assert!(matches!(head.program, Err(Missing::NotFound(_))));
    }

    /// The kernel refuses `file/` with ENOTDIR, so a trailing slash must not
    /// be folded away into a runnable spelling.
    #[cfg(unix)]
    #[test]
    fn a_trailing_slash_on_a_file_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "tool");
        let shell = crate::test_helper::core_shell();
        let head = tmp.path().join(name).to_string_lossy().into_owned() + "/";
        let head = Head::resolve(&CommandName::Path(head), &shell.context);
        assert!(matches!(head.program, Err(Missing::NotFound(_))));
    }

    /// There is nothing to judge in an absent file: classification lets the
    /// head through, and `vet` names the missing command.
    #[test]
    fn a_head_with_no_program_is_admitted_under_any_grant() {
        let ctx = under(ExecGrant::default());
        let head = std::env::temp_dir().join("no-such-dir").join("tool");
        let head = Head::resolve(&path_head(&head), &ctx);
        assert!(admits_head(&ctx, head));
    }

    /// A bare name runs what the walk found, never a same-named file in the
    /// cwd.
    #[test]
    fn a_bare_name_missing_from_path_is_not_found_beside_a_cwd_file() {
        crate::path::forget_located_commands();
        let here = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let name = plant(here.path(), "zzcwdfile");

        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(
            crate::path::CwdPath::new(here.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );
        shell
            .context
            .set_env_var("PATH", elsewhere.path().to_string_lossy().into_owned());

        let head = Head::resolve(&CommandName::Bare(name.into()), &shell.context);
        assert!(matches!(head.program, Err(Missing::NotFound(_))));
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
        let mut shell = crate::test_helper::core_shell();
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
        let mut shell = crate::test_helper::core_shell();
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
        let mut ctx = under(ExecGrant::of([ExecKey::Name("git".into())], []));
        ctx.set_env_var("PATH", dir.path().to_string_lossy().into_owned());

        let head = Head::resolve(&CommandName::Bare("git".into()), &ctx);
        assert_eq!(file_of(&head).1, &canonical(&dir.path().join(name)));
        assert!(!admits_head(&ctx, head));
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

    fn dir_key(dir: &Path) -> ExecKey {
        ExecKey::Dir(FrozenPath::from_surface(crate::path::utf8(dir).unwrap()))
    }

    /// A policy that denies `bash` by bare name yet allows all of its bin
    /// dir: the file that runs is `bash` whatever the head spelled, so the
    /// veto beats the dir.
    #[test]
    fn a_bare_deny_beats_the_allow_dir_a_path_head_sits_in() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "bash");
        let ctx = under(ExecGrant::of(
            [dir_key(tmp.path())],
            [ExecKey::Name("bash".into())],
        ));

        let head = Head::resolve(&path_head(&tmp.path().join(name)), &ctx);
        assert!(!admits_head(&ctx, head));
    }

    /// The mirror: a bare `rg: allow` is the host's `rg`, so a planted path
    /// head `evil/rg` does not inherit it.
    #[test]
    fn a_bare_allow_does_not_admit_a_planted_path_head() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "rg");
        let ctx = under(ExecGrant::of([ExecKey::Name("rg".into())], []));

        let head = Head::resolve(&path_head(&tmp.path().join(name)), &ctx);
        assert!(!admits_head(&ctx, head));
    }

    /// The symlink half of the deny-by-name evasion: `planted/b` wears a name
    /// under the allow dir and runs a file the grant denies.  The program is
    /// that file, so the link is denied by its name.
    #[cfg(unix)]
    #[test]
    fn a_symlink_under_an_allow_dir_meets_the_bare_deny_of_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin, planted, link) = planted_symlink(tmp.path());
        let ctx = under(ExecGrant::of(
            [dir_key(&planted)],
            [ExecKey::Name("bash".into())],
        ));

        let head = Head::resolve(&path_head(&link), &ctx);
        assert_eq!(file_of(&head).1, &canonical(&bin.join("bash")));
        assert!(!admits_head(&ctx, head));
    }

    /// The same link, judged by a denied *region* its program lies in.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_deny_dir_is_denied_by_the_file_it_names() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin, planted, link) = planted_symlink(tmp.path());
        let ctx = under(ExecGrant::of([dir_key(&planted)], [dir_key(&bin)]));

        let head = Head::resolve(&path_head(&link), &ctx);
        assert!(!admits_head(&ctx, head));
    }

    /// A path key names a file, so a link to it runs the admitted file.
    #[cfg(unix)]
    #[test]
    fn a_symlink_is_admitted_by_the_path_key_on_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin, _planted, link) = planted_symlink(tmp.path());
        let ctx = under(ExecGrant::of(
            [ExecKey::Path(FrozenPath::from_surface(
                crate::path::utf8(&bin.join("bash")).unwrap(),
            ))],
            [],
        ));

        let head = Head::resolve(&path_head(&link), &ctx);
        assert!(admits_head(&ctx, head));
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

        let ctx = under(ExecGrant::of(
            [dir_key(&planted)],
            [ExecKey::Name("bash".into())],
        ));

        let head = Head::resolve(&path_head(&copy), &ctx);
        assert!(admits_head(&ctx, head));
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

        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(
            crate::path::CwdPath::new(tmp.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );
        shell.context.set_env_var("PATH", "./bin");

        let head = Head::resolve(&CommandName::Bare(name.as_str().into()), &shell.context);
        assert_eq!(file_of(&head).0, bin.join(&name));
    }
}
