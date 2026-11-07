//! An external's life, below the dispatch: [`prepare`] vets the admitted
//! call and builds its launch, [`start`] spawns it, and [`ending`] reads how
//! it ended; each raises its failures one way.  Stdio routing lives in
//! `stdio` and `redirect`, layered on between the first two.

use super::head::{Admission, Head, unrunnable};
use super::vet::validate_argv;
use crate::authority::policy::Admitted;
use crate::authority::sandbox::{Image, Ownership, Refusal, SandboxProjection};
use crate::fact::{Failure, Unrunnable};
use crate::process::{CancelCause, Launch, PgidPolicy, Scope, Spawned, Unstarted, WaitOutcome};
use crate::shell::runtime::{Break, Error, Settled, Shell, Value, sig};

impl From<Refusal> for Break {
    fn from(refusal: Refusal) -> Self {
        sig(refusal.to_string())
    }
}

/// A vetted call built into its launch, not yet started.  `shown` is the
/// name diagnostics and audit report; the launch runs `image` with `args`.
pub(crate) struct Prepared {
    pub(crate) shown: String,
    pub(crate) args: Vec<String>,
    pub(crate) launch: Launch,
    image: Image,
    /// The launch's own sandbox, where it has one.
    #[cfg(unix)]
    projection: Option<SandboxProjection>,
}

/// Read `head`'s verdict, vet `args`, and build the launch under
/// `ownership`.  Confinement can run for minutes on Windows, so the run's
/// scope is read again on the way out: a wall that expired mid-stamp must
/// not be discovered only once the child is already running.
pub(crate) fn prepare(
    head: Head<Admission>,
    args: &[Value],
    ownership: Ownership,
    shell: &Shell,
) -> Settled<Prepared> {
    let Head { shown, program } = head;
    let admitted = program?;
    let args = validate_argv(&shown, args)?;
    let prepared =
        build_launch(shown, admitted, args, ownership, shell).map_err(|refusal| match refusal {
            #[cfg(windows)]
            Refusal::Cancelled(cause) => Break::cancelled(cause, &shell.scope),
            other => Break::from(other),
        })?;
    shell.poll()?;
    Ok(prepared)
}

/// A started call: its name and args, and what was born of it.
pub(crate) struct Started<T = Spawned> {
    pub(crate) shown: String,
    pub(crate) args: Vec<String>,
    pub(crate) born: T,
}

/// Start `prepared` in `pgid`.
pub(crate) fn start(prepared: Prepared, pgid: PgidPolicy) -> Settled<Started> {
    started(prepared, |launch| launch.spawn(pgid))
}

/// Start `prepared` detached, a survivor nothing here can name again.
#[cfg(unix)]
pub(crate) fn start_detached(
    prepared: Prepared,
) -> Settled<Started<crate::process::group::Survivor>> {
    started(prepared, Launch::spawn_detached)
}

/// `prepared`, spawned once by `spawn`.
fn started<T>(
    mut prepared: Prepared,
    spawn: impl FnOnce(&mut Launch) -> Result<T, Unstarted>,
) -> Settled<Started<T>> {
    match spawn(&mut prepared.launch) {
        Ok(born) => Ok(Started {
            shown: prepared.shown,
            args: prepared.args,
            born,
        }),
        Err(unstarted) => Err(unstarted_error(&prepared, unstarted)),
    }
}

/// The one way a launch that made no child becomes the run's error: the
/// program's own failure as the same process failure the head's probe
/// reports, a refusal as itself, a stop as the run's.
fn unstarted_error(prepared: &Prepared, unstarted: Unstarted) -> Break {
    match unstarted {
        Unstarted::Refused(refusal) => refusal.into(),
        #[cfg(unix)]
        Unstarted::Stopped(stop) => stop.into(),
        Unstarted::Program(e) => {
            let withheld = || -> Option<String> {
                #[cfg(unix)]
                if let (
                    Some(projection),
                    Image::File {
                        interpreter: Some(interpreter),
                        ..
                    },
                ) = (&prepared.projection, &prepared.image)
                {
                    return crate::authority::sandbox::interpreter_hint(projection, interpreter);
                }
                None
            };
            at_spawn(&e, &prepared.image, withheld, &prepared.shown).into()
        }
    }
}

/// What `program`'s failure to start `image` means.  A script whose `#!`
/// interpreter its sandbox withholds, as `withheld` words it, is denied
/// permission whether Seatbelt refused the exec or bwrap hid the interpreter
/// (`ENOENT`).  Otherwise `ENOENT` for a file still at its real path is its
/// missing interpreter or loader, and for one gone, or a bundled tool,
/// `program` not found.
fn at_spawn(
    e: &std::io::Error,
    image: &Image,
    withheld: impl FnOnce() -> Option<String>,
    program: &str,
) -> Error {
    use crate::site::{Silent, site};
    use std::io::ErrorKind::{NotFound, PermissionDenied};
    let failed = |why| Error::process(program, Failure::not_runnable(program, why));
    let denied = matches!(e.kind(), PermissionDenied | NotFound)
        .then(withheld)
        .flatten();
    match (denied, e.kind(), image) {
        (Some(hint), ..) => failed(Unrunnable::PermissionDenied).with_hint(hint),
        (
            None,
            NotFound,
            Image::File {
                real, interpreter, ..
            },
        ) if site!(
            Silent::SpawnedImageStat,
            std::fs::symlink_metadata(real.as_path())
        )
        .is_ok() =>
        {
            failed(Unrunnable::MissingInterpreter(
                interpreter.as_deref().map(crate::path::shown),
            ))
        }
        (None, NotFound, _) => Error::process(program, Failure::NotFound),
        (None, ..) => failed(unrunnable(e)),
    }
}

/// How the run ended, from how its child did: a death by one of `cause`'s
/// signals is that cause's stop, success nothing, and any other end the
/// command's failure, with the kernel's denial hint where a sandbox ran it.
pub(crate) fn ending(
    name: &str,
    outcome: WaitOutcome,
    cause: Option<CancelCause>,
    scope: &Scope,
    shell: &Shell,
    pids: impl FnOnce() -> std::collections::HashSet<u32>,
    since: std::time::Instant,
) -> Settled<()> {
    if let Some(cause) = outcome.cancelled_by(cause) {
        return Err(Break::cancelled(cause, scope));
    }
    match outcome.failure() {
        None => Ok(()),
        Some(failure) => {
            Err(with_denials(Error::process(name, failure), shell, pids, since).into())
        }
    }
}

/// Build `admitted`'s launch with `args` under `ownership`, and apply the
/// shell's scoped env, cwd and limits.  The [`Admitted`] is taken and
/// dropped here, so the launch holds no judged program nor its directory
/// handle, only its [`Image`].  Stdio and `pre_exec` hooks are the caller's:
/// they differ between the standalone and pipeline contexts.
///
/// `ownership` reaches the Linux bwrap backend, which ties the envelope to
/// this process's death unless the child is being surrendered, and the guest
/// jail, which seats a surrendered child outside this engine's cgroup tree.
fn build_launch(
    shown: String,
    admitted: Admitted,
    args: Vec<String>,
    ownership: Ownership,
    shell: &Shell,
) -> Result<Prepared, Refusal> {
    // A guest jail is already the guest's sandbox: bwrap needs unprivileged
    // user namespaces and the guest boots with `user.max_user_namespaces = 0`.
    // The in-process guard in `Head::admit` runs either way.
    let jail = crate::process::jail::guest();
    let projection = jail
        .is_none()
        .then(|| SandboxProjection::of(&shell.context.grants, shell.baseline(), Some(&admitted)))
        .flatten();
    let image = Image::of(&admitted);
    // The judged object and its directory handle end here: a launch holds a
    // name, never the object it was admitted for.
    drop(admitted);
    // A `grant` body evaluates in this process unconfined; spawned children
    // are the only thing an OS sandbox reaches, and this is where it does.
    let mut launch = match &projection {
        Some(projection) => {
            crate::authority::sandbox::projection_enforceable(projection)?;
            crate::authority::sandbox::sandboxed_command(
                projection,
                shell.baseline(),
                &image,
                &args,
                ownership,
                shell.context.cwd(),
                &shell.scope,
            )?
        }
        None => image.launch(&args)?,
    };
    apply_env(&mut launch, shell);
    #[cfg(unix)]
    launch.limit_session(shell.session.child_limits);
    // Hooks run in order: a grant's limits apply last and win.
    if shell.has_active_capabilities() {
        launch.limit_resources()?;
    }
    #[cfg(seccomp)]
    if let Some(jail) = jail {
        let plan = jail
            .plan(ownership, shell.context.grants.net_veto())
            .map_err(crate::process::jail::refused)?;
        launch.apply_guest_jail(&plan)?;
    }
    Ok(Prepared {
        shown,
        args,
        launch,
        image,
        #[cfg(unix)]
        projection,
    })
}

/// `err` with the kernel's denial hint, when a sandboxed child failed on its
/// own account.  Only then are `pids` sampled: an unsandboxed failure never
/// pays for the process-table walk.
fn with_denials(
    err: Error,
    shell: &Shell,
    pids: impl FnOnce() -> std::collections::HashSet<u32>,
    since: std::time::Instant,
) -> Error {
    if shell.sandbox_projection().is_none() {
        return err;
    }
    let exited = match err.reason.failure() {
        Some(&crate::fact::Failure::Exited(n)) => Some(n),
        _ => None,
    };
    if let Some(hint) = crate::authority::sandbox::denial_hint(&pids(), since, exited) {
        return err.append_hint(hint);
    }
    #[cfg(target_os = "macos")]
    if let Some(hint) = exited.and_then(crate::process::limits::failure_hint) {
        return err.append_hint(hint);
    }
    err
}

/// Thread the shell's env overrides, logical cwd and `PWD` into the
/// child; strip dynamic-loader overrides under an active grant.  `current_dir`
/// is set unconditionally because `cd` moves shell state and leaves the process
/// cwd alone, so an inherited `getcwd(3)` would be the wrong directory.
pub(crate) fn apply_env(cmd: &mut crate::process::Launch, shell: &Shell) {
    for (k, v) in shell.context.env_overrides() {
        cmd.env(k, v);
    }
    let cwd = shell.cwd();
    cmd.current_dir(&cwd);
    cmd.env("PWD", &cwd);
    // ral keeps no previous directory; an inherited `OLDPWD` names the
    // launcher's, and would mislead a `cd -` inside the child.
    cmd.env_remove("OLDPWD");
    if shell.has_active_capabilities() {
        // A loader hook makes an admitted binary run someone else's code, so
        // the grant's judgment about which program may run would mean nothing.
        // On macOS the stakes are higher still: a sandboxed external is a
        // re-exec of ral itself, and dyld honours these before `main` runs, so
        // an injected dylib would execute before the child enters Seatbelt.
        for var in &["LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH"] {
            cmd.env_remove(var);
        }
        for var in dyld_vars(shell) {
            cmd.env_remove(var);
        }
    }
}

/// Every `DYLD_`-prefixed name the child would otherwise see, from this
/// process's environment and from the shell's own overrides.  The whole prefix
/// rather than dyld's current list: the loader owns that namespace, and a name
/// added by a future dyld must not become a hole.
fn dyld_vars(shell: &Shell) -> Vec<std::ffi::OsString> {
    const PREFIX: &str = "DYLD_";
    let inherited = crate::host::vars_os()
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| k.as_encoded_bytes().starts_with(PREFIX.as_bytes()));
    let overridden = shell
        .context
        .env_overrides()
        .iter()
        .filter(|(k, _)| k.starts_with(PREFIX))
        .map(|(k, _)| std::ffi::OsString::from(k));
    inherited.chain(overridden).collect()
}

#[cfg(test)]
mod tests {
    use super::with_denials;
    use crate::fact::{Failure, Reason};
    use crate::shell::runtime::Error;

    #[test]
    fn unsandboxed_failures_keep_raw_exits_without_sampling_pids() {
        let shell = crate::test_helper::core_shell();
        assert!(shell.sandbox_projection().is_none());
        for raw in [128, 384, -1, i32::MIN] {
            let error = with_denials(
                Error::from_reason(Reason::process("tool", Failure::Exited(raw))),
                &shell,
                || panic!("an unsandboxed failure must not sample pids"),
                std::time::Instant::now(),
            );
            assert_eq!(error.reason, Reason::process("tool", Failure::Exited(raw)));
            assert!(error.hint.is_none());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandboxed_fork_brake_hints_read_only_raw_exited_reasons() {
        use crate::authority::policy::{Capabilities, FsPolicy};

        let mut shell = crate::test_helper::core_shell();
        let layer = crate::test_helper::layer(Capabilities {
            fs: Some(FsPolicy::default()),
            ..Capabilities::default()
        });
        shell.with_layer(layer, |shell| {
            assert!(shell.sandbox_projection().is_some());
            for reason in [
                Reason::process("tool", Failure::Exited(128)),
                Reason::process("tool", Failure::Exited(384)),
                Reason::process("tool", Failure::Exited(-1)),
                Reason::process("tool", Failure::Signaled(128)),
                Reason::Raised("exit status 128".into()),
            ] {
                let expected = match reason.failure() {
                    Some(&Failure::Exited(raw)) => crate::process::limits::failure_hint(raw),
                    _ => None,
                };
                let error = with_denials(
                    Error::from_reason(reason.clone()),
                    shell,
                    std::collections::HashSet::new,
                    std::time::Instant::now(),
                );
                assert_eq!(error.reason, reason);
                assert_eq!(error.hint.as_deref(), expected.as_deref());
            }
        });
    }
}

/// What runs is the file that was judged, by its real path and under the
/// spelling the user wrote, however the spelling moves in between.
#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod judged_program {
    use super::super::head::Head;
    use super::{Prepared, prepare};
    use crate::authority::sandbox::Ownership;
    use crate::ir::CommandName;
    use crate::shell::runtime::{Mooring, Shell, Value};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};

    /// A scratch directory holding `tools/`, as the shell's cwd.
    fn workdir() -> (tempfile::TempDir, Shell) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("tools")).unwrap();
        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(
            crate::path::CwdPath::new(tmp.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );
        (tmp, shell)
    }

    /// An executable that prints `who`.
    fn plant(dir: &Path, who: &str) -> PathBuf {
        let path = dir.join(who);
        std::fs::write(&path, format!("#!/bin/sh\necho {who}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// `tools/build`, made to point at `target`.
    fn point_build_at(dir: &Path, target: &Path) -> PathBuf {
        let link = dir.join("tools/build");
        let _ = std::fs::remove_file(&link);
        symlink(target, &link).unwrap();
        link
    }

    fn judge(shell: &mut Shell, args: &[&str]) -> Prepared {
        let head = Head::resolve(&CommandName::Path("tools/build".into()), &shell.context);
        let args: Vec<_> = args.iter().map(|arg| Value::string(*arg)).collect();
        let admitted = head
            .admit(&Mooring::adrift(), shell)
            .expect("an unrestricted shell admits the file");
        prepare(admitted, &args, Ownership::Kept, shell).expect("the file can be run")
    }

    fn stdout_of(prepared: Prepared) -> String {
        let out = prepared.launch.output().expect("the file runs");
        String::from_utf8(out.stdout).unwrap()
    }
    /// The control: the harness tells `a` from `b`, so the next test cannot
    /// pass by the two being alike.
    #[test]
    fn a_judged_link_runs_its_target() {
        for who in ["a", "b"] {
            let (tmp, mut shell) = workdir();
            point_build_at(tmp.path(), &plant(tmp.path(), who));
            assert_eq!(stdout_of(judge(&mut shell, &[])), format!("{who}\n"));
        }
    }

    /// A pipeline's earlier stage may retarget a later one's link after the
    /// head is resolved and before it launches: the judged `a` still runs.
    #[test]
    fn a_link_retargeted_after_judgment_runs_the_file_judged() {
        let (tmp, mut shell) = workdir();
        let (a, b) = (plant(tmp.path(), "a"), plant(tmp.path(), "b"));
        let link = point_build_at(tmp.path(), &a);

        let plan = judge(&mut shell, &[]);
        point_build_at(tmp.path(), &b);
        assert_eq!(
            crate::test_helper::real(&link),
            crate::test_helper::real(&b),
            "the link moved"
        );
        assert_eq!(stdout_of(plan), "a\n");
    }

    /// A `#!` script reached through a link is handed its real path by the
    /// kernel, so the lexical idiom `${0%/*}` (what `dirname "$0"` computes)
    /// finds its siblings.
    #[test]
    fn a_script_reached_through_a_link_sees_its_real_path() {
        let (tmp, mut shell) = workdir();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let tool = bin.join("tool");
        std::fs::write(
            &tool,
            "#!/bin/sh\nprintf '%s\\n' \"$0\"; read -r l < \"${0%/*}/sibling\"; printf '%s\\n' \"$l\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(bin.join("sibling"), "SIBLING\n").unwrap();
        point_build_at(tmp.path(), &tool);

        let plan = judge(&mut shell, &[]);
        let real = std::fs::canonicalize(&tool).unwrap();
        assert_eq!(stdout_of(plan), format!("{}\nSIBLING\n", real.display()));
    }

    #[test]
    fn a_program_sees_the_spelling_it_was_named_by_as_argv0() {
        let (tmp, mut shell) = workdir();
        let link = point_build_at(tmp.path(), Path::new("/bin/sh"));

        let plan = judge(&mut shell, &["-c", r#"printf %s "$0""#]);
        assert_eq!(stdout_of(plan), link.to_string_lossy());
    }

    /// The residual, stated: a program is started by its real name, so a
    /// native ELF replaced at that name between `admit` and the exec is what
    /// runs.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_native_elf_replaced_at_its_name_after_admit_runs_the_replacement() {
        let (tmp, mut shell) = workdir();
        let tool = tmp.path().join("tools/build");
        std::fs::copy("/bin/sh", &tool).unwrap();
        let plan = judge(&mut shell, &["-c", "echo admitted"]);
        std::fs::rename(plant(tmp.path(), "replacement"), &tool).unwrap();
        assert_eq!(stdout_of(plan), "replacement\n");
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod at_spawn_tests {
    use super::at_spawn;
    use crate::authority::sandbox::Image;
    use crate::fact::{Failure, Reason, Unrunnable};
    use std::io::{Error, ErrorKind};
    use std::path::Path;

    fn not_runnable(why: Unrunnable) -> Reason {
        Reason::process("run", Failure::not_runnable("run", why))
    }

    /// The image of `real` as built, its `#!` interpreter read then.
    fn file(real: crate::path::RealPath, interpreter: Option<&str>) -> Image {
        Image::File {
            real,
            arg0: "run".into(),
            interpreter: interpreter.map(Into::into),
        }
    }

    /// `e` starting `image`, under no sandbox that withholds anything.
    fn unwithheld(e: &Error, image: &Image) -> Reason {
        at_spawn(e, image, || None, "run").reason
    }

    fn enoent(image: &Image) -> Reason {
        unwithheld(&Error::from(ErrorKind::NotFound), image)
    }

    /// The interpreter named is the one read when the image was built, never
    /// what the file says by the time it failed.
    #[test]
    fn a_file_still_there_lacks_the_interpreter_its_image_names() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("run");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        let real = crate::test_helper::real(&script);
        assert_eq!(
            enoent(&file(real.clone(), Some("/no/such/interpreter"))),
            not_runnable(Unrunnable::MissingInterpreter(Some(
                "/no/such/interpreter".into()
            )))
        );
        assert_eq!(
            enoent(&file(real, None)),
            not_runnable(Unrunnable::MissingInterpreter(None))
        );
    }

    #[test]
    fn a_vanished_file_or_a_tool_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let gone = file(
            crate::test_helper::place(dir.path().join("gone")),
            Some("/bin/sh"),
        );
        for image in [&gone, &Image::Tool("ls")] {
            assert_eq!(
                enoent(image),
                Reason::process("run", Failure::NotFound),
                "{image:?}"
            );
        }
        assert!(!Path::new(&dir.path().join("gone")).exists());
    }

    #[test]
    fn eacces_is_permission_denied_and_the_rest_is_io() {
        let tool = Image::Tool("ls");
        assert_eq!(
            unwithheld(&Error::from_raw_os_error(libc::EACCES), &tool),
            not_runnable(Unrunnable::PermissionDenied)
        );
        let e = Error::from_raw_os_error(libc::E2BIG);
        assert_eq!(
            unwithheld(&e, &tool),
            not_runnable(Unrunnable::Io(e.to_string()))
        );
    }

    /// An interpreter the launch's own sandbox withholds is denied whether
    /// Seatbelt refused it (`EACCES`) or bwrap hid it (`ENOENT`); with no
    /// such verdict, a hidden interpreter reads as missing.  The verdict is
    /// asked for only on those two.
    #[test]
    fn a_withheld_interpreter_is_denied_however_the_kernel_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("run");
        std::fs::write(&script, "#!/opt/x/bin/python3\n").unwrap();
        let image = file(
            crate::test_helper::real(&script),
            Some("/opt/x/bin/python3"),
        );
        let hint = "its interpreter /opt/x/bin/python3 is not admitted by the grant";
        let withheld = || Some(hint.to_owned());

        for errno in [libc::ENOENT, libc::EACCES] {
            let denied = at_spawn(&Error::from_raw_os_error(errno), &image, withheld, "run");
            assert_eq!(
                denied.reason,
                not_runnable(Unrunnable::PermissionDenied),
                "{errno}"
            );
            assert_eq!(denied.hint.as_deref(), Some(hint), "{errno}");
        }
        let missing = at_spawn(
            &Error::from_raw_os_error(libc::ENOENT),
            &image,
            || None,
            "run",
        );
        assert_eq!(
            missing.reason,
            not_runnable(Unrunnable::MissingInterpreter(Some(
                "/opt/x/bin/python3".into()
            )))
        );
        assert!(missing.hint.is_none(), "{:?}", missing.hint);
        let e2big = Error::from_raw_os_error(libc::E2BIG);
        let other = at_spawn(
            &e2big,
            &image,
            || panic!("only a denial or an absence asks for the verdict"),
            "run",
        );
        assert_eq!(
            other.reason,
            not_runnable(Unrunnable::Io(e2big.to_string()))
        );
        assert!(other.hint.is_none(), "{:?}", other.hint);
    }
}

/// A confined launch that cannot start is a process failure, as an
/// unconfined one is, never a trampoline's exit status.
#[cfg(all(test, target_os = "macos"))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod confined_start {
    use super::super::head::Head;
    use super::{prepare, start};
    use crate::authority::policy::{Capabilities, ExecGrant, ExecKey, FsPolicy};
    use crate::authority::sandbox::Ownership;
    use crate::fact::{Failure, Reason, Unrunnable};
    use crate::ir::CommandName;
    use crate::path::FrozenPath;
    use crate::process::PgidPolicy;
    use crate::shell::runtime::{Break, Error, Mooring};
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    /// `body` as an executable `run` in a fresh directory.
    fn script(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let script = std::fs::canonicalize(dir.path()).unwrap().join("run");
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, script)
    }

    /// `script` started under a grant reading its directory and admitting
    /// `exec`, with `between` run after the launch is built: the error.
    fn started(script: &Path, exec: Option<ExecGrant>, between: impl FnOnce()) -> (String, Error) {
        let mut shell = crate::test_helper::core_shell();
        let layer = crate::test_helper::layer(Capabilities {
            exec,
            fs: Some(FsPolicy {
                read_prefixes: vec![FrozenPath::freeze(script.parent().unwrap()).unwrap()],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        });
        shell.with_layer(layer, |shell| {
            let head = Head::resolve(
                &CommandName::Path(script.to_string_lossy().into_owned()),
                &shell.context,
            )
            .admit(&Mooring::adrift(), shell)
            .expect("the grant admits the script");
            let prepared = prepare(head, &[], Ownership::Kept, shell).expect("the launch builds");
            let shown = prepared.shown.clone();
            between();
            let Err(Break::Error(error)) = start(prepared, PgidPolicy::Inherit) else {
                panic!("the confined launch must not start");
            };
            (shown, error)
        })
    }

    fn not_runnable(shown: &str, why: Unrunnable) -> Reason {
        Reason::process(shown, Failure::not_runnable(shown, why))
    }

    #[test]
    fn a_confined_script_whose_interpreter_is_missing_is_missing_interpreter() {
        let (_dir, script) = script("#!/no/such/interpreter\n");
        let (shown, error) = started(&script, None, || ());
        assert_eq!(
            error.reason,
            not_runnable(
                &shown,
                Unrunnable::MissingInterpreter(Some("/no/such/interpreter".into()))
            )
        );
    }

    #[test]
    fn a_confined_file_that_lost_its_execute_bit_is_not_runnable() {
        let (_dir, script) = script("#!/bin/sh\nexit 0\n");
        let (shown, error) = started(&script, None, || {
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        });
        assert_eq!(
            error.reason,
            not_runnable(&shown, Unrunnable::PermissionDenied)
        );
        assert!(error.hint.is_none(), "{:?}", error.hint);
    }

    /// The hint names the interpreter read through the judged handle before
    /// the spawn, not what the script names by the time the kernel refuses.
    #[test]
    fn a_refused_interpreters_hint_names_the_one_read_before_the_spawn() {
        let (_dir, script) = script("#!/bin/sh\n");
        let exec = ExecGrant::of([ExecKey::Path(FrozenPath::freeze(&script).unwrap())], []);
        let (shown, error) = started(&script, Some(exec), || {
            std::fs::write(&script, "#!/usr/bin/false\n").unwrap();
        });
        assert_eq!(
            error.reason,
            not_runnable(&shown, Unrunnable::PermissionDenied)
        );
        let hint = error.hint.as_deref().unwrap_or_default();
        assert!(
            hint.contains("its interpreter /bin/sh is not admitted"),
            "{hint}"
        );
        assert!(!hint.contains("/usr/bin/false"), "{hint}");
    }
}
