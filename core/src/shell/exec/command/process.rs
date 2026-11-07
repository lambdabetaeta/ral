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
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

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
    /// The `#!` interpreter `image` names, read with it: the parent's alone,
    /// to word a failure to start it.
    interpreter: Option<PathBuf>,
    /// The launch's own sandbox, where it has one.
    #[cfg(unix)]
    projection: Option<SandboxProjection>,
    /// Whether an OS sandbox or the guest jail confines the child.
    confined: bool,
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
pub(crate) struct Started<T = Born> {
    pub(crate) shown: String,
    pub(crate) args: Vec<String>,
    pub(crate) born: T,
}

/// A kept child, and the deny lines its failure may claim.
pub(crate) struct Born {
    pub(crate) spawned: Spawned,
    pub(crate) claim: Claim,
}

/// The host pids whose kernel deny lines a child's failure owns.  A line
/// carries only the pid it denied, so each pid here was read while its
/// process lived: what died before, or was born after, goes unclaimed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    /// No sandbox confined the child: no line is its.
    Unconfined,
    /// A confined child's pids, as the spawn knew them.
    Confined(HashSet<u32>),
}

impl Claim {
    /// What a launch confined as `confined` claims of `spawned`, taken the
    /// moment it started: a direct child, its own pid; an enveloped one, the
    /// envelope's chain while its program runs, whose host pids no process
    /// inside the namespace can see.
    fn of(confined: bool, enveloped: bool, spawned: &Spawned) -> Self {
        match (confined, enveloped, spawned.landing) {
            (false, ..) => Self::Unconfined,
            (true, false, _) => Self::Confined(HashSet::from([spawned.child.id()])),
            #[cfg(target_os = "linux")]
            (true, true, crate::process::Landing::Owns(payload)) => {
                Self::Confined(crate::authority::sandbox::payload_pids(payload))
            }
            // `Launch::spawn` lands an envelope in its payload's group.
            (true, true, _) => Self::Confined(HashSet::new()),
        }
    }
}

/// Start `prepared` in `pgid`, and claim what it bore.
pub(crate) fn start(prepared: Prepared, pgid: PgidPolicy) -> Settled<Started> {
    let (confined, enveloped) = (prepared.confined, prepared.launch.enveloped());
    started(prepared, |launch| {
        let spawned = launch.spawn(pgid)?;
        let claim = Claim::of(confined, enveloped, &spawned);
        Ok(Born { spawned, claim })
    })
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
            let interpreter = prepared.interpreter.as_deref();
            let withheld = || -> Option<String> {
                #[cfg(unix)]
                if let (Some(projection), Some(interpreter)) = (&prepared.projection, interpreter) {
                    return crate::authority::sandbox::interpreter_hint(projection, interpreter);
                }
                None
            };
            at_spawn(&e, &prepared.image, interpreter, withheld, &prepared.shown).into()
        }
    }
}

/// What `program`'s failure to start `image`, whose `#!` names `interpreter`,
/// means.  A script whose interpreter its sandbox withholds, as `withheld`
/// words it, is denied permission whether Seatbelt refused the exec or bwrap
/// hid the interpreter (`ENOENT`).  Otherwise `ENOENT` for a file still at
/// its real path is its missing interpreter or loader, and for one gone, or a
/// bundled tool, `program` not found.
fn at_spawn(
    e: &std::io::Error,
    image: &Image,
    interpreter: Option<&Path>,
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
        (None, NotFound, Image::File { real, .. })
            if site!(
                Silent::SpawnedImageStat,
                std::fs::symlink_metadata(real.as_path())
            )
            .is_ok() =>
        {
            failed(Unrunnable::MissingInterpreter(
                interpreter.map(crate::path::shown),
            ))
        }
        (None, NotFound, _) => Error::process(program, Failure::NotFound),
        (None, ..) => failed(unrunnable(e)),
    }
}

/// How the run ended, from how its child did: a death by one of `cause`'s
/// signals is that cause's stop, success nothing, and any other end the
/// command's failure, with the kernel's account of the lines `claim` owns
/// since `since`.
pub(crate) fn ending(
    name: &str,
    outcome: WaitOutcome,
    cause: Option<CancelCause>,
    scope: &Scope,
    claim: &Claim,
    since: Instant,
) -> Settled<()> {
    if let Some(cause) = outcome.cancelled_by(cause) {
        return Err(Break::cancelled(cause, scope));
    }
    match outcome.failure() {
        None => Ok(()),
        Some(failure) => Err(with_denials(name, failure, claim, since).into()),
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
    let confined = projection.is_some() || jail.is_some();
    let image = Image::of(&admitted);
    let interpreter = crate::authority::sandbox::interpreter_of(&admitted);
    // The judged object and its directory handle end here: a launch holds a
    // name, never the object it was admitted for.
    drop(admitted);
    // A `grant` body evaluates in this process unconfined; spawned children
    // are the only thing an OS sandbox reaches, and this is where it does.
    let mut launch = match &projection {
        Some(projection) => crate::authority::sandbox::sandboxed_command(
            projection,
            &image,
            &args,
            ownership,
            shell.context.cwd(),
            &shell.scope,
        )?,
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
            .plan(ownership, shell.context.grants.net())
            .map_err(crate::process::jail::refused)?;
        launch.apply_guest_jail(&plan)?;
    }
    Ok(Prepared {
        shown,
        args,
        launch,
        image,
        interpreter,
        #[cfg(unix)]
        projection,
        confined,
    })
}

/// `name`'s `failure` as its error, with, where a sandbox confined it, the
/// kernel's account: the deny lines since `since` that `claim` owns, else,
/// on macOS, the fork brake's for an exit status of 128.  An unconfined
/// failure reads no log.
fn with_denials(name: &str, failure: Failure, claim: &Claim, since: Instant) -> Error {
    let Claim::Confined(pids) = claim else {
        return Error::process(name, failure);
    };
    let exited = match failure {
        Failure::Exited(n) => Some(n),
        _ => None,
    };
    let hint = crate::authority::sandbox::denial_hint(pids, since, exited);
    #[cfg(target_os = "macos")]
    let hint = hint.or_else(|| exited.and_then(crate::process::limits::failure_hint));
    let error = Error::process(name, failure);
    match hint {
        Some(hint) => error.with_hint(hint),
        None => error,
    }
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
    use super::{Claim, with_denials};
    use crate::fact::{Failure, Reason};
    use std::time::Instant;

    /// No line is an unconfined child's: its failure is the raw ending, with
    /// no kernel account and no brake blamed, whatever the status.
    #[test]
    fn unconfined_failures_keep_raw_exits_without_a_hint() {
        for failure in [
            Failure::Exited(128),
            Failure::Exited(384),
            Failure::Exited(5),
            Failure::Exited(0xC000_0022_u32.cast_signed()),
            Failure::Exited(-1),
            Failure::Exited(i32::MIN),
            Failure::Signaled(31),
        ] {
            let error = with_denials("tool", failure.clone(), &Claim::Unconfined, Instant::now());
            assert_eq!(error.reason, Reason::process("tool", failure));
            assert!(error.hint.is_none(), "{:?}", error.hint);
        }
    }

    /// The brake's hint is decided from the failure itself: an exit status
    /// of 128, never a death by signal 128 nor any other status.  A claim of
    /// no pid reads no log, so the brake's is the only hint here.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_confined_fork_brake_hint_reads_only_an_exit_status_of_128() {
        let braked = crate::process::limits::failure_hint(128);
        assert!(braked.is_some());
        let claim = Claim::Confined(std::collections::HashSet::new());
        for (failure, hint) in [
            (Failure::Exited(128), braked.as_deref()),
            (Failure::Exited(384), None),
            (Failure::Exited(-1), None),
            (Failure::Signaled(128), None),
        ] {
            let error = with_denials("tool", failure.clone(), &claim, Instant::now());
            assert_eq!(error.reason, Reason::process("tool", failure));
            assert_eq!(error.hint.as_deref(), hint);
        }
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

    /// No sandbox confines a start under no grant, so it claims no line.
    #[test]
    fn an_unconfined_start_claims_no_line() {
        use super::{Born, Claim, Started, start};
        let (tmp, mut shell) = workdir();
        point_build_at(tmp.path(), &plant(tmp.path(), "a"));
        let mut prepared = judge(&mut shell, &[]);
        prepared.launch.stdout(crate::process::StdioSpec::Null);
        let Ok(Started {
            born: Born { mut spawned, claim },
            ..
        }) = start(prepared, crate::process::PgidPolicy::Inherit)
        else {
            panic!("the file starts");
        };
        let _ = spawned.child.reap();
        assert_eq!(claim, Claim::Unconfined);
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

    fn file(real: crate::path::RealPath) -> Image {
        Image::File {
            real,
            arg0: "run".into(),
        }
    }

    /// `e` starting `image`, whose `#!` named `interpreter` when it was
    /// built, under no sandbox that withholds anything.
    fn unwithheld(e: &Error, image: &Image, interpreter: Option<&str>) -> Reason {
        at_spawn(e, image, interpreter.map(Path::new), || None, "run").reason
    }

    fn enoent(image: &Image, interpreter: Option<&str>) -> Reason {
        unwithheld(&Error::from(ErrorKind::NotFound), image, interpreter)
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
            enoent(&file(real.clone()), Some("/no/such/interpreter")),
            not_runnable(Unrunnable::MissingInterpreter(Some(
                "/no/such/interpreter".into()
            )))
        );
        assert_eq!(
            enoent(&file(real), None),
            not_runnable(Unrunnable::MissingInterpreter(None))
        );
    }

    #[test]
    fn a_vanished_file_or_a_tool_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let gone = file(crate::test_helper::place(dir.path().join("gone")));
        for image in [&gone, &Image::Tool("ls")] {
            assert_eq!(
                enoent(image, Some("/bin/sh")),
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
            unwithheld(&Error::from_raw_os_error(libc::EACCES), &tool, None),
            not_runnable(Unrunnable::PermissionDenied)
        );
        let e = Error::from_raw_os_error(libc::E2BIG);
        assert_eq!(
            unwithheld(&e, &tool, None),
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
        let image = file(crate::test_helper::real(&script));
        let interpreter = Some(Path::new("/opt/x/bin/python3"));
        let hint = "its interpreter /opt/x/bin/python3 is not admitted by the grant";
        let withheld = || Some(hint.to_owned());

        for errno in [libc::ENOENT, libc::EACCES] {
            let denied = at_spawn(
                &Error::from_raw_os_error(errno),
                &image,
                interpreter,
                withheld,
                "run",
            );
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
            interpreter,
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
            interpreter,
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
    use super::{Born, Claim, Started, prepare, start};
    use crate::authority::policy::{Capabilities, ExecGrant, ExecKey, FsPolicy};
    use crate::authority::sandbox::Ownership;
    use crate::fact::{Failure, Reason, Unrunnable};
    use crate::ir::CommandName;
    use crate::path::FrozenPath;
    use crate::process::PgidPolicy;
    use crate::shell::runtime::{Break, Error, Mooring, Settled};
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
    /// `exec`, with `between` run after the launch is built.
    fn launched(
        script: &Path,
        exec: Option<ExecGrant>,
        between: impl FnOnce(),
    ) -> (String, Settled<Started>) {
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
            (shown, start(prepared, PgidPolicy::Inherit))
        })
    }

    /// [`launched`], which must not start: the error.
    fn started(script: &Path, exec: Option<ExecGrant>, between: impl FnOnce()) -> (String, Error) {
        let (shown, settled) = launched(script, exec, between);
        let Err(Break::Error(error)) = settled else {
            panic!("the confined launch must not start");
        };
        (shown, error)
    }

    /// A confined direct child is its program, `execve`d in place: it claims
    /// its own pid, and no other.
    #[test]
    fn a_confined_start_claims_exactly_its_childs_pid() {
        let (_dir, script) = script("#!/bin/sh\nexit 0\n");
        let (_, settled) = launched(&script, None, || ());
        let Ok(Started {
            born: Born { mut spawned, claim },
            ..
        }) = settled
        else {
            panic!("the confined script starts");
        };
        let pid = spawned.child.id();
        let _ = spawned.child.reap();
        assert_eq!(
            claim,
            Claim::Confined(std::collections::HashSet::from([pid]))
        );
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
