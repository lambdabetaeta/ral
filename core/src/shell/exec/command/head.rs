//! A [`Head`] freezes the shown spelling and its [`Target`] without I/O.
//! [`Head::admit`] judges each candidate before it is looked at, and reads
//! 127/126 off that judged walk for `super::vet` to report.

use crate::authority::guard::{Denial, check_exec, check_exec_name};
use crate::authority::policy::{Admitted, Baseline, GrantStack, Program};
use crate::ir::CommandName;
use crate::path::walk::failure_trace;
use crate::path::{self, CwdPath, Kind, LeafMode, LexicalPath, walk};
use crate::process::{SpawnFailure, Unrunnable, path_as_text_hint};
use crate::shell::runtime::{Break, Context, Error, Mooring, Settled, Shell};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct Head<P = Target> {
    /// A path head is shown as typed, not folded by [`path::shown`].
    pub(crate) shown: String,
    pub(crate) program: P,
}

pub(crate) type Admission = Result<Admitted, Missing>;

#[derive(Debug)]
pub(crate) enum Target {
    Tool(&'static str),
    /// A path head's launch path; `None` where it names none.
    Path(Option<PathBuf>),
    /// A bare name's `PATH` and cwd, frozen at dispatch.
    Search {
        path: Option<String>,
        cwd: CwdPath,
    },
}

pub(crate) struct Denied {
    pub(crate) at: String,
    pub(crate) denial: Denial,
}

#[derive(Clone, Debug)]
pub(crate) enum Missing {
    /// No admitted program; `meant` is a runnable Windows sibling's spelling.
    NotFound { meant: Option<String> },
    /// An admitted non-program; `None` means permission denied.
    NotExecutable {
        found: PathBuf,
        kind: Option<Unrunnable>,
    },
}

impl Head {
    /// Freeze `name` against `ctx`, without consulting the disk.
    pub(crate) fn resolve(name: &CommandName, ctx: &Context) -> Self {
        match name {
            CommandName::Bare(bare) => {
                let shown = bare.to_string();
                let program = crate::uutils::bundled_name(std::ffi::OsStr::new(bare.as_ref()))
                    .map_or_else(
                        || Target::Search {
                            path: ctx.env_overrides().get_or_host("PATH"),
                            cwd: ctx.cwd().clone(),
                        },
                        Target::Tool,
                    );
                Self { shown, program }
            }
            CommandName::Path(path) => Self {
                shown: path.clone(),
                program: Target::Path(Some(launch_path(&PathBuf::from(path), ctx))),
            },
            CommandName::TildePath(path) => match ctx.home() {
                Some(home) => Self {
                    shown: path.shown(&home),
                    program: Target::Path(Some(launch_path(&path.expand(&home), ctx))),
                },
                None => Self {
                    shown: path.to_literal(),
                    program: Target::Path(None),
                },
            },
        }
    }

    /// Select the first admitted program, retaining the first non-absence.
    pub(crate) fn select(
        self,
        grants: &GrantStack,
        baseline: &Baseline,
    ) -> Result<Head<Admission>, Denied> {
        let Self { shown, program } = self;
        let program = match program {
            Target::Tool(tool) => check_exec(grants, baseline, &shown, Program::Tool(tool))
                .map(Ok)
                .map_err(|denial| Denied {
                    at: shown.clone(),
                    denial,
                }),
            Target::Path(Some(file)) => {
                let mut admission =
                    probe(file.clone(), CandidateKind::Path, &shown, grants, baseline)?;
                if cfg!(windows)
                    && let Err(Missing::NotFound { meant }) = &mut admission
                {
                    *meant = path::WINDOWS_EXEC_EXTENSIONS
                        .iter()
                        .find(|ext| {
                            matches!(
                                probe(
                                    path::with_appended_suffix(&file, ext),
                                    CandidateKind::Path,
                                    &shown,
                                    grants,
                                    baseline,
                                ),
                                Ok(Ok(_))
                            )
                        })
                        .map(|ext| format!("{shown}.{ext}"));
                }
                Ok(admission)
            }
            Target::Search { path, cwd } => {
                if let Some(hit) = path::locate(&shown, path.as_deref(), Some(&cwd))
                    && let Ok(Ok(admitted)) =
                        probe(hit, CandidateKind::Search, &shown, grants, baseline)
                {
                    Ok(Ok(admitted))
                } else {
                    select_candidates(
                        path::command_candidates(&shown, path.as_deref(), Some(&cwd)),
                        &shown,
                        grants,
                        baseline,
                    )
                }
            }
            Target::Path(None) => Ok(Err(Missing::NOT_FOUND)),
        }?;
        Ok(Head { shown, program })
    }

    /// Select under the live stack, recording a refusal against `mooring`.
    pub(crate) fn admit(self, mooring: &Mooring, shell: &mut Shell) -> Settled<Head<Admission>> {
        let search = matches!(&self.program, Target::Search { .. });
        let shown = self.shown.clone();
        self.select(&shell.context.grants, shell.baseline())
            .map_err(
                |Denied { denial, .. }| match shell.exec_refused(denial, mooring) {
                    Break::Error(e) if search => Break::Error(e.prepend_hint(format!(
                        "'{shown}' is no program in any PATH directory the grant runs from"
                    ))),
                    refusal => path_as_text(refusal, &shown),
                },
            )
    }
}

fn select_candidates(
    candidates: impl IntoIterator<Item = PathBuf>,
    shown: &str,
    grants: &GrantStack,
    baseline: &Baseline,
) -> Result<Admission, Denied> {
    let mut first = None;
    for candidate in candidates {
        let failure = match probe(candidate, CandidateKind::Search, shown, grants, baseline) {
            Ok(Ok(admitted)) => return Ok(Ok(admitted)),
            Ok(Err(Missing::NotFound { .. })) => continue,
            Ok(Err(missing)) => Ok(missing),
            Err(denied) => Err(denied),
        };
        first.get_or_insert(failure);
    }
    first.unwrap_or(Ok(Missing::NOT_FOUND)).map(Err)
}

#[derive(Clone, Copy)]
enum CandidateKind {
    Path,
    Search,
}

fn probe(
    arg0: PathBuf,
    candidate_kind: CandidateKind,
    shown: &str,
    grants: &GrantStack,
    baseline: &Baseline,
) -> Result<Admission, Denied> {
    let Some(spelled) = LexicalPath::absolute(&arg0) else {
        return Ok(Err(Missing::NOT_FOUND));
    };
    let located = match walk(&spelled, LeafMode::Follow) {
        Ok(located) => located,
        Err(error) => {
            if let Ok(trace) = failure_trace(&spelled, &error) {
                check_exec_name(grants, baseline, shown, &trace).map_err(|denial| Denied {
                    at: path::shown(&arg0),
                    denial,
                })?;
            }
            return Ok(Err(match (candidate_kind, error.kind()) {
                (_, ErrorKind::NotFound | ErrorKind::NotADirectory)
                | (CandidateKind::Search, _) => Missing::NOT_FOUND,
                (CandidateKind::Path, _) => Missing::NotExecutable {
                    found: arg0,
                    kind: None,
                },
            }));
        }
    };
    let at = path::shown(&arg0);
    let admitted = check_exec(
        grants,
        baseline,
        shown,
        Program::File {
            arg0,
            located: Box::new(located),
        },
    )
    .map_err(|denial| Denied { at, denial })?;
    let Program::File { arg0, located } = admitted.program() else {
        return Ok(Ok(admitted));
    };
    if located.is_executable() {
        return Ok(Ok(admitted));
    }
    let missing = match located.kind() {
        Some(kind) if matches!(candidate_kind, CandidateKind::Path) || kind == Kind::File => {
            Missing::NotExecutable {
                found: arg0.clone(),
                kind: Some(if kind == Kind::Dir {
                    Unrunnable::Directory
                } else {
                    Unrunnable::File
                }),
            }
        }
        _ => Missing::NOT_FOUND,
    };
    Ok(Err(missing))
}

/// `refusal`, led by the path as text when `shown` is a path head.
fn path_as_text(refusal: Break, shown: &str) -> Break {
    match refusal {
        Break::Error(e) if path::names_a_path(shown) => {
            Break::Error(e.prepend_hint(path_as_text_hint(shown, None)))
        }
        other => other,
    }
}

#[cfg(all(test, unix))]
impl Program {
    /// The present file `path` names, walked but unjudged.
    pub(crate) fn file(path: PathBuf) -> Option<Self> {
        let spelled = LexicalPath::absolute(&path)?;
        let located = walk(&spelled, LeafMode::Follow).ok()?;
        located.trace().kind()?;
        Some(Self::File {
            arg0: path,
            located: Box::new(located),
        })
    }
}

impl Missing {
    const NOT_FOUND: Self = Self::NotFound { meant: None };

    /// Format the judged walk's 127/126 verdict without probing again.
    pub(crate) fn error(&self, shown: &str) -> Error {
        match self {
            Self::NotFound { meant } => {
                let error = Error::spawn_failure(shown, SpawnFailure::NotFound);
                match meant {
                    Some(meant) => error.with_hint(format!("did you mean '{meant}'?")),
                    None => error,
                }
            }
            Self::NotExecutable { found, kind } => Error::spawn_failure(
                shown,
                match kind {
                    Some(kind) => SpawnFailure::NotAProgram {
                        found: (!path::names_a_path(shown))
                            .then(|| found.clone().into_boxed_path()),
                        kind: *kind,
                    },
                    None => SpawnFailure::PermissionDenied {
                        found: Some(found.clone().into_boxed_path()),
                    },
                },
            ),
        }
    }
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

    fn admits_head(ctx: &Context, head: Head) -> bool {
        head.select(&ctx.grants, &Baseline::default()).is_ok()
    }

    fn selected(head: Head) -> Head<Admission> {
        head.select(&GrantStack::root(), &Baseline::default())
            .unwrap_or_else(|_| panic!("an unrestricted stack admits the head"))
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

    fn file_of(head: &Head<Admission>) -> (&Path, &crate::path::RealPath) {
        match head.program.as_ref().map(Admitted::program) {
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

        let head = selected(Head::resolve(
            &CommandName::Path(format!("./{name}")),
            &shell.context,
        ));
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

        let head = selected(Head::resolve(
            &CommandName::Path("link/../tool".into()),
            &shell.context,
        ));
        let (path, judged) = file_of(&head);
        assert_eq!(judged, &canonical(&real.join("tool")));
        assert_eq!(path, tmp.path().join("link/../tool"));
    }

    #[test]
    fn a_path_head_naming_no_file_has_no_program() {
        let shell = crate::test_helper::core_shell();
        let head = std::env::temp_dir().join("no-such-dir").join("configure");
        let head = selected(Head::resolve(&path_head(&head), &shell.context));
        assert!(matches!(head.program, Err(Missing::NotFound { .. })));
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
        let head = selected(Head::resolve(&CommandName::Path(head), &shell.context));
        assert!(matches!(head.program, Err(Missing::NotFound { .. })));
    }

    #[test]
    fn an_empty_exec_allow_list_refuses_a_nonexistent_path_head() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = under(ExecGrant::default());
        for file in [tmp.path().join("missing"), tmp.path().join("absent/tool")] {
            let head = Head::resolve(&path_head(&file), &ctx);
            let denied = head
                .select(&ctx.grants, &Baseline::default())
                .expect_err("an empty exec allowance refuses even an absent name");
            assert_eq!(denied.at, path::shown(&file));
            assert!(denied.denial.error.message.contains("exec `allow` list"));
        }
    }

    #[test]
    fn read_withheld_heads_are_refused_like_present_programs() {
        use crate::authority::policy::FsPolicy;
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join(plant(tmp.path(), "zzhidden"));
        #[cfg(unix)]
        let looping = {
            let looping = tmp.path().join("loop.bat");
            std::os::unix::fs::symlink("loop.bat", &looping).unwrap();
            looping
        };
        let ctx = Context::under_grants(crate::test_helper::stack([crate::test_helper::layer(
            Capabilities {
                fs: Some(FsPolicy::default()),
                ..Capabilities::default()
            },
        )]));
        let denied = |file: &Path| {
            Head::resolve(&path_head(file), &ctx)
                .select(&ctx.grants, &Baseline::default())
                .expect_err("running a read-withheld name is refused")
                .denial
        };
        let cases = [
            tmp.path().join("missing.bat"),
            tmp.path().join("absent/tool.bat"),
            #[cfg(unix)]
            looping.clone(),
        ]
        .map(|file| {
            let refusal = denied(&file);
            assert!(refusal.error.message.contains("not readable"));
            (file, refusal)
        });
        #[cfg(unix)]
        std::fs::rename(&looping, tmp.path().join("saved-loop")).unwrap();
        for (file, before) in cases {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::copy(&source, &file).unwrap();
            let after = denied(&file);
            assert_eq!(before.error, after.error, "{}", file.display());
            assert_eq!(before.check, after.check, "{}", file.display());
        }
    }

    #[test]
    fn bare_search_skips_refused_candidates_including_memo_hits() {
        for prime_memo in [false, true] {
            let d1 = tempfile::tempdir().unwrap();
            let d2 = tempfile::tempdir().unwrap();
            let name = plant(d1.path(), "zzadmitted");
            plant(d2.path(), "zzadmitted");
            let path = std::env::join_paths([d1.path(), d2.path()])
                .unwrap()
                .into_string()
                .unwrap();
            let mut ctx = under(ExecGrant::of([dir_key(d2.path())], []));
            ctx.set_env_var("PATH", path.clone());
            let head = Head::resolve(&CommandName::Bare(name.as_str().into()), &ctx);
            if prime_memo {
                assert_eq!(
                    path::locate(&name, Some(&path), Some(ctx.cwd())),
                    Some(d1.path().join(&name))
                );
            }
            let head = head
                .select(&ctx.grants, &Baseline::default())
                .unwrap_or_else(|_| panic!("the second PATH candidate is admitted"));
            assert_eq!(file_of(&head).0, d2.path().join(&name));
        }
    }

    #[cfg(unix)]
    #[test]
    fn path_order_decides_between_refusal_and_non_executability() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let admitted = tmp.path().join("admitted");
        let denied = tmp.path().join("denied");
        std::fs::create_dir(&admitted).unwrap();
        std::fs::create_dir(&denied).unwrap();
        let plain = admitted.join("zzplain");
        std::fs::write(&plain, "").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        let ctx = under(ExecGrant::of([dir_key(&admitted)], []));
        let absent = admitted.join("absent/zzplain");
        let refused = denied.join("zzplain");
        let baseline = Baseline::default();
        let first = select_candidates(
            [absent.clone(), refused.clone(), plain.clone()],
            "zzplain",
            &ctx.grants,
            &baseline,
        )
        .expect_err("the first non-absence is a refusal");
        assert_eq!(first.at, path::shown(&refused));
        let first = select_candidates(
            [absent, plain.clone(), refused],
            "zzplain",
            &ctx.grants,
            &baseline,
        )
        .unwrap_or_else(|_| panic!("the non-executable file precedes the refusal"));
        assert!(matches!(first, Err(Missing::NotExecutable { found, .. }) if found == plain));
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

        let head = selected(Head::resolve(
            &CommandName::Bare(name.into()),
            &shell.context,
        ));
        assert!(matches!(head.program, Err(Missing::NotFound { .. })));
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

        let head = selected(Head::resolve(
            &CommandName::Bare("zznoexec".into()),
            &shell.context,
        ));
        assert!(matches!(head.program, Err(Missing::NotExecutable { found, .. }) if found == file));
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
        assert!(matches!(head.program, Target::Tool(tool) if tool == "ls"));
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
        assert_eq!(
            file_of(&selected(Head::resolve(
                &CommandName::Bare("git".into()),
                &ctx
            )))
            .1,
            &canonical(&dir.path().join(name)),
        );
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
        assert_eq!(
            file_of(&selected(Head::resolve(&path_head(&link), &ctx))).1,
            &canonical(&bin.join("bash")),
        );
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

        let head = selected(Head::resolve(
            &CommandName::Bare(name.as_str().into()),
            &shell.context,
        ));
        assert_eq!(file_of(&head).0, bin.join(&name));
    }
}
