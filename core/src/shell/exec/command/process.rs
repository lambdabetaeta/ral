//! An external's life, below the dispatch: [`prepare`] vets the admitted
//! call and builds its launch, [`start`] spawns it, and [`ending`] reads how
//! it ended; each raises its failures one way.  Stdio routing lives in
//! `stdio` and `redirect`, layered on between the first two.

use super::head::{Admission, Head};
use super::vet::{SpawnPlan, vet};
use crate::authority::policy::Admitted;
use crate::authority::sandbox::{Image, Ownership, Refusal};
use crate::fact::{Failure, NotRunnable, Unrunnable};
use crate::process::{CancelCause, Launch, PgidPolicy, Scope, Spawned, Unstarted, WaitOutcome};
use crate::shell::runtime::{Break, Error, Settled, Shell, Value, sig};

impl From<Refusal> for Break {
    fn from(refusal: Refusal) -> Self {
        sig(refusal.to_string())
    }
}

/// A vetted call built into its launch, not yet started.  `shown` is the
/// name diagnostics and audit report; the launch runs with `args`.
pub(crate) struct Prepared {
    pub(crate) shown: String,
    pub(crate) args: Vec<String>,
    pub(crate) launch: Launch,
    /// The hint naming the script's `#!` interpreter that the launch's own
    /// sandbox withholds, where it does.
    interpreter_hint: Option<String>,
}

/// Vet the admitted `head` and `args`, and build the launch under `ownership`.
/// Confinement can run for minutes on Windows, so the run's scope is read
/// again on the way out: a wall that expired mid-stamp must not be
/// discovered only once the child is already running.
pub(crate) fn prepare(
    head: Head<Admission>,
    args: &[Value],
    ownership: Ownership,
    shell: &Shell,
) -> Settled<Prepared> {
    let SpawnPlan {
        shown,
        admitted,
        args,
    } = vet(head, args)?;
    let (launch, interpreter_hint) =
        build_launch(admitted, &args, ownership, shell).map_err(|refusal| match refusal {
            #[cfg(windows)]
            Refusal::Cancelled(cause) => Break::cancelled(cause, &shell.scope),
            other => Break::from(other),
        })?;
    shell.poll()?;
    Ok(Prepared {
        shown,
        args,
        launch,
        interpreter_hint,
    })
}

/// Start `prepared` in `pgid`.
pub(crate) fn start(prepared: &mut Prepared, pgid: PgidPolicy) -> Settled<Spawned> {
    prepared
        .launch
        .spawn(pgid)
        .map_err(|unstarted| unstarted_error(prepared, unstarted))
}

/// Start `prepared` detached, a survivor nothing here can name again.
#[cfg(unix)]
pub(crate) fn start_detached(prepared: &mut Prepared) -> Settled<crate::process::group::Survivor> {
    prepared
        .launch
        .spawn_detached()
        .map_err(|unstarted| unstarted_error(prepared, unstarted))
}

/// The one way a launch that made no child becomes the run's error: the
/// program's own failure as the same process failure `vet`'s probe reports,
/// a refusal as itself, a stop as the run's.
fn unstarted_error(prepared: &Prepared, unstarted: Unstarted) -> Break {
    match unstarted {
        Unstarted::Refused(refusal) => refusal.into(),
        #[cfg(unix)]
        Unstarted::Stopped(stop) => stop.into(),
        Unstarted::Program(e) => unstarted_program(
            &e,
            prepared.interpreter_hint.as_deref(),
            prepared.launch.image(),
            &prepared.shown,
        )
        .into(),
    }
}

/// `program`'s own failure to start `image`.  A script whose `#!`
/// interpreter its sandbox withholds, as `interpreter_hint` says, is denied
/// permission whether Seatbelt refused the exec or bwrap hid the interpreter
/// (`ENOENT`); anything else is as [`at_spawn`] reads it.
fn unstarted_program(
    e: &std::io::Error,
    interpreter_hint: Option<&str>,
    image: Option<&Image>,
    program: &str,
) -> Error {
    use std::io::ErrorKind::{NotFound, PermissionDenied};
    match interpreter_hint.filter(|_| matches!(e.kind(), PermissionDenied | NotFound)) {
        Some(hint) => Error::process(program, not_runnable(program, Unrunnable::PermissionDenied))
            .with_hint(hint),
        None => Error::process(program, at_spawn(e, image, program)),
    }
}

/// What a failed spawn of `image` means for `program`.  `ENOENT` for a file
/// still at its real path is its missing interpreter or loader; for one gone,
/// or a bundled tool, it is `program` not found.
fn at_spawn(e: &std::io::Error, image: Option<&Image>, program: &str) -> Failure {
    use crate::site::{Silent, site};

    if e.kind() != std::io::ErrorKind::NotFound {
        return not_runnable(program, Unrunnable::of_io(e));
    }
    match image {
        Some(Image::File {
            real, interpreter, ..
        }) if site!(
            Silent::SpawnedImageStat,
            std::fs::symlink_metadata(real.as_path())
        )
        .is_ok() =>
        {
            not_runnable(
                program,
                Unrunnable::MissingInterpreter(interpreter.as_deref().map(crate::path::shown)),
            )
        }
        _ => Failure::NotFound,
    }
}

fn not_runnable(program: &str, why: Unrunnable) -> Failure {
    Failure::NotRunnable(NotRunnable {
        found: program.to_owned(),
        why,
    })
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

/// Build a launch for the `admitted` call and apply the shell's scoped env,
/// cwd and limits; beside it, the hint for a `#!` interpreter its sandbox
/// withholds.  The [`Admitted`] is taken and dropped here, so the launch
/// holds no judged program nor its directory handle, only its [`Image`].
/// Stdio and `pre_exec` hooks are the caller's: they differ between the
/// standalone and pipeline contexts.
///
/// `ownership` reaches the Linux bwrap backend, which ties the envelope to
/// this process's death unless the child is being surrendered, and the guest
/// jail, which seats a surrendered child outside this engine's cgroup tree.
fn build_launch(
    admitted: Admitted,
    args: &[String],
    ownership: Ownership,
    shell: &Shell,
) -> Result<(Launch, Option<String>), Refusal> {
    // A guest jail is already the guest's sandbox: bwrap needs unprivileged
    // user namespaces and the guest boots with `user.max_user_namespaces = 0`.
    // The in-process guard in `Head::admit` runs either way.
    let jail = crate::process::jail::guest();
    let projection = jail.is_none().then(|| {
        crate::authority::sandbox::SandboxProjection::of(
            &shell.context.grants,
            shell.baseline(),
            Some(&admitted),
        )
    });
    let image = Image::of(&admitted);
    // The judged object and its directory handle end here: a launch holds a
    // name, never the object it was admitted for.
    drop(admitted);
    // A `grant` body evaluates in this process unconfined; spawned children
    // are the only thing an OS sandbox reaches, and this is where it does.
    let (mut launch, interpreter_hint) = match projection.flatten() {
        Some(projection) => {
            crate::authority::sandbox::projection_enforceable(&projection)?;
            let launch = crate::authority::sandbox::sandboxed_command(
                &projection,
                shell.baseline(),
                &image,
                args,
                ownership,
                shell.context.cwd(),
                &shell.scope,
            )?;
            (launch, withheld_interpreter(&projection, &image))
        }
        None => (image.launch(args)?, None),
    };
    launch.set_image(image);
    apply_env(&mut launch, shell);
    #[cfg(unix)]
    launch.limit_session(shell.session.child_limits);
    // Hooks run in order: a grant's limits apply last and win.
    if shell.has_active_capabilities() {
        launch.limit_resources()?;
    }
    #[cfg(seccomp)]
    if let Some(jail) = jail {
        let plan = (jail.plan(ownership, shell.context.grants.net_veto()))
            .map_err(crate::process::jail::refused)?;
        launch.apply_guest_jail(&plan)?;
    }
    Ok((launch, interpreter_hint))
}

/// The hint for `image`'s `#!` interpreter, where `projection` withholds it.
#[cfg_attr(
    windows,
    allow(unused_variables, reason = "no Windows image names an interpreter")
)]
fn withheld_interpreter(
    projection: &crate::authority::sandbox::SandboxProjection,
    image: &Image,
) -> Option<String> {
    #[cfg(unix)]
    if let Image::File {
        interpreter: Some(interpreter),
        ..
    } = image
    {
        return crate::authority::sandbox::interpreter_hint(projection, interpreter);
    }
    None
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
    use super::{at_spawn, unstarted_program};
    use crate::authority::sandbox::Image;
    use crate::fact::{Failure, NotRunnable, Reason, Unrunnable};
    use std::io::{Error, ErrorKind};
    use std::path::Path;

    fn not_runnable(why: Unrunnable) -> Failure {
        Failure::NotRunnable(NotRunnable {
            found: "run".into(),
            why,
        })
    }

    /// The image of `real` as built, its `#!` interpreter read then.
    fn file(real: crate::path::RealPath, interpreter: Option<&str>) -> Image {
        Image::File {
            real,
            arg0: "run".into(),
            interpreter: interpreter.map(Into::into),
        }
    }

    fn enoent(image: Option<&Image>) -> Failure {
        at_spawn(&Error::from(ErrorKind::NotFound), image, "run")
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
            enoent(Some(&file(real.clone(), Some("/no/such/interpreter")))),
            not_runnable(Unrunnable::MissingInterpreter(Some(
                "/no/such/interpreter".into()
            )))
        );
        assert_eq!(
            enoent(Some(&file(real, None))),
            not_runnable(Unrunnable::MissingInterpreter(None))
        );
    }

    #[test]
    fn a_vanished_file_a_tool_or_no_image_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let gone = file(
            crate::test_helper::place(dir.path().join("gone")),
            Some("/bin/sh"),
        );
        for image in [Some(&gone), Some(&Image::Tool("ls")), None] {
            assert_eq!(enoent(image), Failure::NotFound, "{image:?}");
        }
        assert!(!Path::new(&dir.path().join("gone")).exists());
    }

    #[test]
    fn eacces_is_permission_denied_and_the_rest_is_io() {
        assert_eq!(
            at_spawn(&Error::from_raw_os_error(libc::EACCES), None, "run"),
            not_runnable(Unrunnable::PermissionDenied)
        );
        let e = Error::from_raw_os_error(libc::E2BIG);
        assert_eq!(
            at_spawn(&e, None, "run"),
            not_runnable(Unrunnable::Io(e.to_string()))
        );
    }

    /// An interpreter the launch's own sandbox withholds is denied whether
    /// Seatbelt refused it (`EACCES`) or bwrap hid it (`ENOENT`); with no
    /// such verdict, a hidden interpreter reads as missing.
    #[test]
    fn a_withheld_interpreter_is_denied_however_the_kernel_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("run");
        std::fs::write(
            &script,
            "#!/opt/x/bin/python3
",
        )
        .unwrap();
        let image = file(
            crate::test_helper::real(&script),
            Some("/opt/x/bin/python3"),
        );
        let hint = "its interpreter /opt/x/bin/python3 is not admitted by the grant";
        let started = |e: &Error, verdict| unstarted_program(e, verdict, Some(&image), "run");
        let reason = |why| Reason::process("run", not_runnable(why));

        for errno in [libc::ENOENT, libc::EACCES] {
            let denied = started(&Error::from_raw_os_error(errno), Some(hint));
            assert_eq!(
                denied.reason,
                reason(Unrunnable::PermissionDenied),
                "{errno}"
            );
            assert_eq!(denied.hint.as_deref(), Some(hint), "{errno}");
        }
        let missing = started(&Error::from_raw_os_error(libc::ENOENT), None);
        assert_eq!(
            missing.reason,
            reason(Unrunnable::MissingInterpreter(Some(
                "/opt/x/bin/python3".into()
            )))
        );
        assert!(missing.hint.is_none(), "{:?}", missing.hint);
        let e2big = Error::from_raw_os_error(libc::E2BIG);
        let other = started(&e2big, Some(hint));
        assert_eq!(other.reason, reason(Unrunnable::Io(e2big.to_string())));
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
    use crate::fact::{Failure, NotRunnable, Reason, Unrunnable};
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
            let mut prepared =
                prepare(head, &[], Ownership::Kept, shell).expect("the launch builds");
            between();
            let Err(Break::Error(error)) = start(&mut prepared, PgidPolicy::Inherit) else {
                panic!("the confined launch must not start");
            };
            (prepared.shown, error)
        })
    }

    fn not_runnable(shown: &str, why: Unrunnable) -> Reason {
        Reason::process(
            shown,
            Failure::NotRunnable(NotRunnable {
                found: shown.into(),
                why,
            }),
        )
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
