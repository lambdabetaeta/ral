use super::*;
use crate::environ::ChildEnv;

/// `sh -c script` confined under `unrestricted()` through the path a
/// real command takes, `Launch::spawn` and then the collector, the group
/// the spawn named, and a shell whose current scope is `scope`, for the
/// collector to read.  Callers skip first where no envelope builds.
fn spawn_confined(
    script: &str,
    scope: &crate::process::Scope,
) -> (
    crate::process::Spawned,
    crate::process::Pgid,
    crate::shell::runtime::Shell,
) {
    use crate::process::{Group, PgidPolicy};
    let mut shell = crate::test_helper::core_shell();
    shell.scope = scope.clone();
    let launch = crate::authority::sandbox::Confinement::Projected(Box::new(unrestricted()))
        .launch(
            &Image::of(&sh()),
            &["-c".into(), script.into()],
            Ownership::Kept,
            shell.context.cwd(),
        )
        .expect("build confined launch");
    let spawned = launch
        .spawn(PgidPolicy::NewLeader, scope)
        .expect("spawn confined sh");
    let Group::Enveloped(pgid) = spawned.group else {
        panic!("a kept launch names its payload's group");
    };
    (spawned, pgid, shell)
}

/// The whole regression: `xdg:config` readable, `xdg:config/gcloud`
/// denied, and no gcloud ever installed — the mask over that absent name
/// had bwrap `mkdir` a mountpoint on a read-only bind, and its `EROFS`
/// killed every external command under the grant.  The options were
/// well-formed throughout, so only a spawn catches it.
#[test]
fn an_absent_deny_under_a_read_only_bind_still_lets_the_body_run() {
    let dir = workdir("deny-absent-spawn");
    let config = dir.join("config");
    let work = dir.join("work");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let denied = config.join("gcloud");

    let policy = |deny: &[&std::path::Path]| restricted(&[&config], &[&work], deny);
    let Some(env) = envelope_launches(&policy(&[])) else {
        return;
    };

    let script = format!(
        "echo READY\n\
         mkdir -p '{denied}' 2>/dev/null || echo DENY-CREATE-REFUSED\n",
        denied = denied.display(),
    );
    let out = run_confined(&env, &policy(&[&denied]), &script).expect("spawn bwrap");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("READY"),
        "an absent deny stopped the envelope from launching: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("DENY-CREATE-REFUSED"),
        "the read-only bind must still refuse creating the denied name: {stdout}"
    );
    assert!(
        !denied.exists(),
        "the denied name was created on the host: {}",
        denied.display()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Building an envelope must leave the host tree exactly as it found it.
/// Masking an absent deny under a *writable* identity bind had bwrap
/// `mkdir` its mountpoint straight onto the host: one launch under
/// `deny: ['cwd:/.env']` and the user's project held an `.env` directory
/// their own tools could then never write.  Only a spawn sees it — the
/// options were well-formed, and the mount was correct inside the
/// namespace.
#[test]
fn building_an_envelope_never_creates_a_denied_name_on_the_host() {
    let dir = workdir("deny-absent-spawn-rw");
    let denied = dir.join("not-yet");

    let Some(env) = envelope_launches(&deny_within(&dir, &[])) else {
        return;
    };
    let out =
        run_confined(&env, &deny_within(&dir, &[&denied]), "echo READY").expect("spawn bwrap");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("READY"),
        "an absent deny stopped the envelope from launching: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !denied.exists(),
        "building the envelope created the denied name on the host: {}",
        denied.display()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// No options assertion can catch a mask that makes bwrap exit before it
/// execs anything, so this one spawns the envelope too.
#[test]
fn a_denied_path_refuses_every_access_while_the_body_still_runs() {
    let dir = workdir("deny-spawn");
    let key = dir.join("id_rsa");
    let git = dir.join(".git");
    let readable = dir.join("README");
    std::fs::write(&key, "PRIVATE-KEY-BYTES").unwrap();
    std::fs::create_dir_all(&git).unwrap();
    std::fs::write(git.join("config"), "GIT-CONFIG-BYTES").unwrap();
    std::fs::write(&readable, "README-BYTES").unwrap();

    let Some(env) = envelope_launches(&deny_within(&dir, &[])) else {
        return;
    };
    let script = format!(
        "echo READY\n\
         cat '{key}' 2>/dev/null || echo KEY-READ-REFUSED\n\
         echo pwned > '{key}' 2>/dev/null || echo KEY-WRITE-REFUSED\n\
         cat '{config}' 2>/dev/null || echo GIT-READ-REFUSED\n\
         touch '{planted}' 2>/dev/null || echo GIT-WRITE-REFUSED\n\
         chmod 700 '{git}' 2>/dev/null; touch '{planted}' 2>/dev/null\n\
         cat '{readable}' 2>/dev/null\n",
        key = key.display(),
        config = git.join("config").display(),
        git = git.display(),
        planted = git.join("planted").display(),
        readable = readable.display(),
    );
    let out = run_confined(&env, &deny_within(&dir, &[&key, &git]), &script).expect("spawn bwrap");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stdout.contains("READY"),
        "the deny masks stopped the envelope from launching, so nothing was confined: {stderr}"
    );
    assert!(
        !stdout.contains("PRIVATE-KEY-BYTES"),
        "a denied file's bytes reached the child: {stdout}"
    );
    assert!(
        !stdout.contains("GIT-CONFIG-BYTES"),
        "a denied directory's contents reached the child: {stdout}"
    );
    for refusal in [
        "KEY-READ-REFUSED",
        "KEY-WRITE-REFUSED",
        "GIT-READ-REFUSED",
        "GIT-WRITE-REFUSED",
    ] {
        assert!(stdout.contains(refusal), "missing {refusal}: {stdout}");
    }
    assert!(
        stdout.contains("README-BYTES"),
        "the grant's own writable prefix stopped being readable: {stdout}"
    );

    // The script's last move is the mask's known limit: a child that
    // chmods the tmpfs it owns can write inside it.  What must hold is
    // that none of it reaches the host, which the two assertions below
    // are — the deny is over the real directory, not over the memory a
    // child spends on believing otherwise.
    assert_eq!(
        std::fs::read_to_string(&key).unwrap(),
        "PRIVATE-KEY-BYTES",
        "the denied file was overwritten on the host"
    );
    assert!(
        !git.join("planted").exists(),
        "a child planted a file inside a denied directory"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Only a spawn shows the trampoline's `execve` reaching the file the user
/// named: `~/.local/bin/uv`, a link into `~/.local/share/uv`.  The image is
/// shown by the reads alone, at its real path: a grant that reads it runs
/// it by either name, and one that does not has the `execve` fail, the
/// image being absent inside.
#[test]
fn a_linked_image_runs_through_the_read_that_shows_its_real_path() {
    let (dir, real) = linked_tool("image-link-spawn");
    std::fs::write(&real, "#!/bin/sh\necho READY\n").unwrap();
    let elsewhere = workdir("image-link-spawn-elsewhere");
    let tool = || Program::file(dir.join("linkdir/tool")).expect("the tool exists");
    let reading = restricted(&[&dir.join("real")], &[], &[]);
    let Some(env) = envelope_launches(&reading) else {
        return;
    };
    let out = run_admitted(&env, &reading, &admitted(tool()), &[]).expect("spawn bwrap");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("READY"),
        "the spelled name did not run inside the envelope: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let apart = deny_within(&elsewhere, &[]);
    let ran = run_admitted(&env, &apart, &admitted(tool()), &[]);
    assert!(
        matches!(ran, Err(Unstarted::Program(_))),
        "an image no read shows ran: {ran:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&elsewhere);
}

/// The handles are opened when the launch is built, so a prefix swapped
/// for a symlink before the spawn still shows what the grant named.
#[test]
fn a_prefix_swapped_for_a_symlink_after_the_open_shows_what_was_opened() {
    let dir = workdir("swap-race");
    let (data, elsewhere) = (dir.join("data"), dir.join("elsewhere"));
    for (at, bytes) in [(&data, "ORIGINAL-BYTES"), (&elsewhere, "SWAPPED-BYTES")] {
        std::fs::create_dir(at).unwrap();
        std::fs::write(at.join("f"), bytes).unwrap();
    }
    let policy = restricted(&[&data], &[], &[]);
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let script = format!("echo READY\ncat '{}'\n", data.join("f").display());
    let out = run_after(&env, &policy, &sh(), &sh_args(&script), || {
        std::fs::rename(&data, dir.join("moved")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &data).unwrap();
    })
    .expect("spawn bwrap");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("READY") && stdout.contains("ORIGINAL-BYTES"),
        "the opened prefix was not what the child saw: {stdout} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !stdout.contains("SWAPPED-BYTES"),
        "the swap redirected the mount: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A destination is still a name bwrap resolves inside: one the host
/// turned into a symlink after the open kills the launch, never moves
/// the mount.
#[test]
fn a_destination_turned_into_a_symlink_after_the_open_runs_nothing() {
    let dir = workdir("dest-link");
    let (outer, inner) = (dir.join("outer"), dir.join("outer/inner"));
    std::fs::create_dir_all(&inner).unwrap();
    let policy = restricted(&[&outer], &[&inner], &[]);
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let ran = run_after(&env, &policy, &sh(), &sh_args("echo READY"), || {
        std::fs::rename(&inner, outer.join("moved")).unwrap();
        std::os::unix::fs::symlink("moved", &inner).unwrap();
    });
    assert!(
        matches!(ran, Err(Unstarted::Refused(_))),
        "a mount followed a symlinked destination: {ran:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Forced rather than probed: where `--dev` mounts, nothing else would
/// exercise this arm.
#[test]
fn the_by_hand_dev_serves_a_confined_body() {
    let dir = workdir("dev-by-hand");
    let policy = deny_within(&dir, &[]);
    let Some(env) = envelope_launches(&policy) else {
        return;
    };

    let by_hand = Envelope {
        host: HostEnvelope {
            virtual_dev: false,
            ..env.host
        },
        ..env
    };
    let out = run_confined(
        &by_hand,
        &policy,
        "echo x > /dev/null && echo NULL-OK\n\
         head -c 1 /dev/urandom > /dev/null && echo URANDOM-OK\n\
         : > /dev/shm/probe && echo SHM-OK\n",
    )
    .expect("spawn bwrap");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    for ok in ["NULL-OK", "URANDOM-OK", "SHM-OK"] {
        assert!(
            stdout.contains(ok),
            "the by-hand /dev is missing {ok}: {stdout} {stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A host terminal: both ends held open, and the slave's name.
struct HostPty {
    _master: std::os::fd::OwnedFd,
    slave: std::os::fd::OwnedFd,
    name: String,
}

impl HostPty {
    fn open() -> Self {
        use std::os::fd::{FromRawFd, OwnedFd};
        let (mut master, mut slave) = (-1, -1);
        let mut name = [0; 128];
        // SAFETY: outputs and the terminal-name buffer are writable.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    name.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0,
            "host openpty: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: openpty returned two fresh descriptors and a terminated name.
        unsafe {
            Self {
                _master: OwnedFd::from_raw_fd(master),
                slave: OwnedFd::from_raw_fd(slave),
                name: std::ffi::CStr::from_ptr(name.as_ptr())
                    .to_str()
                    .unwrap()
                    .to_owned(),
            }
        }
    }
}

/// A selected owned tty must still be visible to bwrap's console discovery,
/// even when the test runner's own stdout is a pipe or a file.
#[test]
fn an_owned_stdout_terminal_remains_the_envelopes_console() {
    use crate::process::{PgidPolicy, StdioSpec, WaitOutcome};
    pin_envelope();
    let bwrap = match envelope() {
        Ok(bwrap) => bwrap,
        Err(why) => {
            skip(why);
            return;
        }
    };
    let env = Envelope::probe(bwrap, crate::authority::sandbox::reexec::own().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let policy = deny_within(dir.path(), &[]);
    let script = if env.host.virtual_dev {
        "test -t 1 && test /proc/self/fd/1 -ef /dev/console"
    } else {
        "test -t 1"
    };
    let mut launch = enveloped(
        &env,
        &policy,
        &Image::of(&sh()),
        &["-c".into(), script.into()],
        None,
        Ownership::Kept,
    )
    .expect("build a tty launch");
    let terminal = HostPty::open();
    launch
        .stdin(StdioSpec::Null)
        .stdout(StdioSpec::from_owned(terminal.slave.try_clone().unwrap()))
        .stderr(StdioSpec::Null);
    let spawned = launch
        .spawn(
            PgidPolicy::NewLeader,
            &crate::process::DurableRoot::default().child(),
        )
        .expect("spawn with owned tty stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    let watch = spawned.child.into_watch(tx, std::convert::identity);
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(5));
    if outcome.is_err() {
        if let Some(pgid) = spawned.group.owned() {
            pgid.kill();
        }
        watch.kill();
    }
    watch.reap().expect("reap tty launch");
    assert_eq!(outcome, Ok(WaitOutcome::Exited(0)));
}

/// By hand no host terminal is reachable by name, whether open at render or
/// opened after it, and none can be allocated; the terminal the payload
/// inherits, the late one, still works.
#[test]
fn the_by_hand_dev_reaches_no_host_terminal() {
    use crate::test_helper::confinement::{Output, denied, probe};
    let proof = crate::require_confinement!();
    let early = HostPty::open();
    let dir = tempfile::tempdir().unwrap();
    let policy = deny_within(dir.path(), &[]);
    pin_envelope();
    let mut env = Envelope::probe(
        envelope().expect("confinement requires bwrap"),
        crate::authority::sandbox::reexec::own().unwrap(),
    );
    env.host.virtual_dev = false;
    let invoke = |args: &dyn Fn(&str) -> Vec<String>| -> Output {
        let program = admitted(Program::file(std::env::current_exe().unwrap()).unwrap());
        let mut launch = enveloped(
            &env,
            &policy,
            &Image::of(&program),
            &[],
            None,
            Ownership::Kept,
        )
        .unwrap();
        let late = HostPty::open();
        let args = args(&late.name);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (cwd, request) = (crate::test_helper::process_cwd(), probe::request(&args));
        launch
            .stdin(crate::process::StdioSpec::from_owned(
                late.slave.try_clone().unwrap(),
            ))
            .situate(ChildEnv {
                cwd: cwd.as_path(),
                set: vec![("RAL_TEST_PROBE".as_ref(), request.as_ref())],
                unset: Vec::new(),
            });
        launch.output().unwrap().into()
    };
    let control = invoke(&|_| vec!["tty".into()]);
    for named in [
        invoke(&|_| vec!["pty".into(), early.name.clone()]),
        invoke(&|late| vec!["pty".into(), late.into()]),
    ] {
        denied(&proof, &control, &named, "ok");
        assert!(
            named.stderr.contains("No such file or directory"),
            "{named:?}"
        );
    }
    denied(&proof, &control, &invoke(&|_| vec!["pty".into()]), "ok");
}

/// What the options cannot show: that a program inside reads its own limits
/// where its `/proc/self/cgroup` says they are, and finds no interface
/// the netns it is in does not have.
#[test]
fn a_confined_program_reads_its_own_cgroup_and_none_of_the_hosts_interfaces() {
    use std::os::unix::fs::MetadataExt;
    let restricted = deny_within(&workdir("sys-spawn"), &[]);
    let script = "ls /sys/class/net >/dev/null 2>&1 && echo NET-VISIBLE\n\
                  echo NPROC=$(nproc)\n\
                  echo CGROUP=$(cat /proc/self/cgroup)\n\
                  echo TREE=$(stat -c %i /sys/fs/cgroup)\n";
    for (label, policy) in [
        ("unrestricted", &unrestricted()),
        ("restricted", &restricted),
    ] {
        let Some(env) = envelope_launches(policy) else {
            continue;
        };
        let host = env.host;
        let (stdout, _) = confined(&env, policy, script);
        if label == "restricted" {
            assert!(
                !emitted(&stdout, "NET-VISIBLE"),
                "the host's interfaces were listed under a restricted fs: {stdout}"
            );
        }
        let nproc: u32 = field(&stdout, "NPROC=")
            .and_then(|n| n.parse().ok())
            .expect("a cpu count");
        assert!(
            nproc > 0,
            "{label}: a program must still count its cpus: {stdout}"
        );
        let Some(own) = own_cgroup() else {
            continue;
        };
        let expected = if host.private_cgroup {
            assert_eq!(
                field(&stdout, "CGROUP="),
                Some("0::/"),
                "{label}: the payload must be the root of its own namespace: {stdout}"
            );
            format!("/sys/fs/cgroup{own}")
        } else {
            "/sys/fs/cgroup".to_string()
        };
        let expected = std::fs::metadata(&expected)
            .expect("ral's own cgroup")
            .ino();
        assert_eq!(
            field(&stdout, "TREE=").and_then(|n| n.parse().ok()),
            Some(expected),
            "{label}: /sys/fs/cgroup inside must be the tree /proc/self/cgroup names: {stdout}"
        );
    }
}

/// T3 (design doc §6): the grace signal must reach the confined payload's
/// own session, not bwrap's mortal monitor.  Spawns through the same
/// `Launch` + collector path a real command takes, so the `--info-fd`
/// leader in `Launch::spawn` is exercised end to end, then cancels with
/// `Explicit` and checks the trap ran well inside `TEARDOWN_GRACE`.
///
/// The trap's `exit 0` makes the outcome a plain exit; the file is the
/// witness that the signal arrived.  `Unrestricted`: the ladder keys on
/// there being an envelope, not on which axis was attenuated.
#[test]
fn a_confined_payload_gets_its_grace_signal_not_the_monitors() {
    use crate::process::{CancelCause, DurableRoot};

    if envelope_launches(&unrestricted()).is_none() {
        return;
    }

    let dir = workdir("receipt-grace");
    let flag = dir.join("grace");
    let ready = dir.join("ready");
    let script = format!(
        "trap 'echo GRACE > {} ; exit 0' TERM\n: > {}\nsleep 30\n",
        flag.display(),
        ready.display()
    );

    let striker = DurableRoot::default().child();
    let (spawned, _, shell) = spawn_confined(&script, &striker);

    let canceller = std::thread::spawn(move || {
        // The marker follows the trap's installation; a miss fails the asserts.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        striker.cancel(CancelCause::Cancelled);
    });

    let t0 = std::time::Instant::now();
    let (outcome, cause) = crate::shell::exec::pipeline::lone_end(spawned, &shell);
    let elapsed = t0.elapsed();
    canceller.join().expect("canceller thread");

    assert!(
        elapsed.as_secs() < 5,
        "teardown must not fall back to the payload's own 30 s sleep: took {elapsed:?}"
    );
    assert_eq!(
        cause,
        Some(CancelCause::Cancelled),
        "the cancel must have reached this wait"
    );
    assert_eq!(
        outcome,
        crate::process::WaitOutcome::Exited(0),
        "the trap's own exit must be reported, not a SIGKILL of an \
         unreached payload"
    );
    assert_eq!(
        std::fs::read_to_string(&flag).unwrap_or_default().trim(),
        "GRACE",
        "the trap must have run: the grace signal reached the payload \
         itself, not just bwrap's monitor"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// bwrap writes `child-pid` before that process's `setsid`, and a kill
/// sent to a group not yet made is lost.  The spawn hands the group out
/// only once it exists, so an abort's one kill, struck before the wait,
/// ends a confined `sleep 30` at once.
#[test]
fn a_confined_payloads_group_exists_when_its_spawn_returns() {
    use crate::process::{CancelCause, DurableRoot};

    if envelope_launches(&unrestricted()).is_none() {
        return;
    }
    let scope = DurableRoot::default().child();
    let (spawned, pgid, shell) = spawn_confined("sleep 30", &scope);
    assert!(
        rustix::process::test_kill_process_group(pgid.as_pid()).is_ok(),
        "the payload's group {pgid} must exist once its spawn returns"
    );

    scope.cancel(CancelCause::Aborted);
    let t0 = std::time::Instant::now();
    let (outcome, _) = crate::shell::exec::pipeline::lone_end(spawned, &shell);
    let elapsed = t0.elapsed();
    assert!(
        elapsed.as_secs() < 5,
        "an abort's one kill must end the payload, not wait out its sleep: took {elapsed:?}"
    );
    assert_eq!(
        outcome,
        signaled(libc::SIGKILL),
        "the supervisor, killed with its program, told nothing: bwrap's 137 is its death"
    );
}

/// An enveloped start claims its program by the host pid a deny line would
/// carry, read beneath the payload as it runs; never bwrap's monitor, which
/// no deny-set confines.
#[test]
fn an_enveloped_start_claims_its_running_program_and_not_the_monitor() {
    use std::time::Duration;

    if envelope_launches(&unrestricted()).is_none() {
        return;
    }
    start_granted("exec sleep 30", |started, shell| {
        let Some(pids) = &started.denial_pids else {
            panic!("a launch under a grant is confined");
        };
        let crate::process::Group::Enveloped(payload) = started.spawned.group else {
            panic!("a kept launch names its payload's group");
        };
        let comm = |pid: &u32| std::fs::read_to_string(format!("/proc/{pid}/comm"));
        let program = crate::test_helper::eventually(Duration::from_secs(5), || {
            pids.iter()
                .find(|pid| comm(pid).is_ok_and(|comm| comm.trim_end() == "sleep"))
                .copied()
        });
        let leader = pids.contains(&payload.as_raw().cast_unsigned());
        let monitor = pids.contains(&started.spawned.child.id());
        payload.kill();
        let _ = started.collect(None, std::time::Instant::now(), shell);
        assert!(program.is_some(), "the running program is unclaimed");
        assert!(leader, "the payload's leader is unclaimed");
        assert!(!monitor, "bwrap's monitor is claimed");
    });
}

fn signaled(signal: i32) -> crate::process::WaitOutcome {
    crate::process::WaitOutcome::Signaled(crate::process::Signal::new(signal))
}

/// The program's end as its supervisor saw it, through the reaper as every
/// waiter sees it: a signal death and an exit of `128 + n` stay apart.
fn ended(program: &Admitted, args: &[String]) -> Option<crate::process::WaitOutcome> {
    use crate::process::{PgidPolicy, Spawned};
    envelope_launches(&unrestricted())?;
    let shell = crate::test_helper::core_shell();
    let mut launch = crate::authority::sandbox::Confinement::Projected(Box::new(unrestricted()))
        .launch(
            &Image::of(program),
            &args
                .iter()
                .map(|arg| arg.as_str().into())
                .collect::<Vec<_>>(),
            Ownership::Kept,
            shell.context.cwd(),
        )
        .expect("build confined launch");
    launch.stdin(crate::process::StdioSpec::Null);
    let Spawned { child, .. } = launch
        .spawn(PgidPolicy::NewLeader, &shell.scope)
        .expect("spawn the confined program");
    let (tx, rx) = std::sync::mpsc::channel();
    let watch = child.into_watch(tx, std::convert::identity);
    let outcome = rx.recv().expect("the reaper reports the envelope's exit");
    watch.reap().expect("reap the envelope");
    Some(outcome)
}

#[test]
fn a_confined_programs_signal_death_is_reported_as_one() {
    if let Some(outcome) = ended(&sh(), &sh_args("kill -SEGV $$")) {
        assert_eq!(outcome, signaled(libc::SIGSEGV));
    }
}

#[test]
fn a_confined_programs_exit_of_128_plus_n_is_an_exit() {
    if let Some(outcome) = ended(&sh(), &sh_args("exit 139")) {
        assert_eq!(outcome, crate::process::WaitOutcome::Exited(139));
    }
}

/// `env` execs its program in place: the supervisor that reports its end
/// must not be the process that execs.
#[cfg(feature = "coreutils")]
#[cfg_attr(
    ctor_without_argv,
    ignore = "a test binary serves bundled tools before main, where musl gives std no argv"
)]
#[test]
fn a_confined_tools_exec_of_a_program_exiting_128_plus_n_is_an_exit() {
    let env = admitted(Program::Tool("env"));
    let args = ["/bin/sh", "-c", "exit 130"].map(String::from);
    if let Some(outcome) = ended(&env, &args) {
        assert_eq!(outcome, crate::process::WaitOutcome::Exited(130));
    }
}

/// The program closes all its streams, then waits for an out-of-band
/// acknowledgement. EOF and EPIPE must arrive before that acknowledgement,
/// not only when either the supervisor or bwrap finishes waiting.
fn assert_program_releases_its_streams(program: &Admitted, prefix: &[&str]) {
    pin_envelope();
    let bwrap = match envelope() {
        Ok(bwrap) => bwrap,
        Err(why) => {
            skip(why);
            return;
        }
    };
    let env = Envelope::probe(
        bwrap,
        crate::authority::sandbox::reexec::own().expect("ral pins itself"),
    );
    // An installed envelope failing the control is a failure, not a skip
    // that could hide a broken stream handoff in these regressions.
    let control = run_confined(&env, &unrestricted(), "echo READY")
        .expect("installed bwrap must launch the stream control");
    assert!(control.status.success(), "{control:?}");
    assert_eq!(control.stdout, b"READY\n");
    for open_files in [None, Some(64)] {
        assert_stream_closure(&env, program, prefix, open_files);
        if env.host.private_pids {
            let mut shared = env;
            shared.host.private_pids = false;
            assert_stream_closure(&shared, program, prefix, open_files);
        }
    }
}

fn assert_stream_closure(
    env: &Envelope<'_>,
    program: &Admitted,
    prefix: &[&str],
    open_files: Option<u64>,
) {
    use crate::process::{PgidPolicy, Spawned, StdioSpec, WaitOutcome};
    use rustix::fs::{OFlags, fcntl_setfl};
    use rustix::io::{Errno, read, write};
    use std::sync::mpsc::{TryRecvError, channel};
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().expect("acknowledgement directory");
    let ack = dir.path().join("ack");
    let ready = dir.path().join("closed");
    let script = "printf OUT; printf ERR >&2\n\
                  exec 0<&- 1>&- 2>&-\n\
                  : > \"$2\"\n\
                  while [ ! -e \"$1\" ]; do sleep 0.01; done\n\
                  exit 139";
    let args: Vec<std::borrow::Cow<'_, str>> = prefix
        .iter()
        .copied()
        .chain([
            script,
            "stream-closure",
            ack.to_str().expect("UTF-8 temp path"),
            ready.to_str().expect("UTF-8 temp path"),
        ])
        .map(Into::into)
        .collect();
    let (input, stdin) = crate::process::cloexec_pipe().expect("stdin pipe");
    let (stdout, output) = crate::process::cloexec_pipe().expect("stdout pipe");
    let (stderr, error) = crate::process::cloexec_pipe().expect("stderr pipe");
    fcntl_setfl(&stdin, OFlags::NONBLOCK).expect("nonblocking stdin writer");
    fcntl_setfl(&stdout, OFlags::NONBLOCK).expect("nonblocking stdout reader");
    fcntl_setfl(&stderr, OFlags::NONBLOCK).expect("nonblocking stderr reader");
    let shell = crate::test_helper::core_shell();
    let mut launch = enveloped(
        env,
        &unrestricted(),
        &Image::of(program),
        &args,
        Some(shell.context.cwd()),
        Ownership::Kept,
    )
    .expect("build confined launch");
    launch.limit_session(crate::process::ChildLimits {
        open_files,
        ..Default::default()
    });
    launch
        .stdin(StdioSpec::from_owned(input))
        .stdout(StdioSpec::from_owned(output))
        .stderr(StdioSpec::from_owned(error));
    let Spawned { child, group, .. } = launch
        .spawn(PgidPolicy::NewLeader, &shell.scope)
        .expect("spawn the confined program");
    let (tx, rx) = channel();
    let watch = child.into_watch(tx, std::convert::identity);
    let timeout = Duration::from_secs(5);
    let mut received = [Vec::new(), Vec::new()];
    let closed = (|| {
        let deadline = Instant::now() + timeout;
        let mut closed = [false; 3];
        while (closed != [true; 3] || !ready.exists()) && Instant::now() < deadline {
            if !closed[0] {
                match write(&stdin, b"x") {
                    Err(Errno::PIPE) => closed[0] = true,
                    Ok(_) | Err(Errno::AGAIN | Errno::INTR) => {}
                    Err(e) => return Err(e),
                }
            }
            for ((closed, stream), received) in closed[1..]
                .iter_mut()
                .zip([&stdout, &stderr])
                .zip(&mut received)
            {
                if !*closed {
                    let mut byte = [0];
                    match read(stream, &mut byte) {
                        Ok(0) => *closed = true,
                        Ok(_) => received.extend_from_slice(&byte),
                        Err(Errno::AGAIN | Errno::INTR) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(closed)
    })();
    let reached_close = ready.exists();
    let still_running = matches!(rx.try_recv(), Err(TryRecvError::Empty));
    let outcome = if closed == Ok([true; 3]) && reached_close && still_running {
        std::fs::write(&ack, b"ack")
            .ok()
            .and_then(|()| rx.recv_timeout(timeout).ok())
    } else {
        None
    };
    // Both waits are bounded; kill before reaping on any failure so the
    // regression itself cannot leave a test hanging on the acknowledgement.
    if outcome.is_none() {
        if let Some(pgid) = group.owned() {
            pgid.kill();
        }
        watch.kill();
    }
    watch.reap().expect("reap the envelope");
    assert_eq!(closed, Ok([true; 3]), "stdin EPIPE, stdout EOF, stderr EOF");
    assert_eq!(
        received,
        [b"OUT".to_vec(), b"ERR".to_vec()],
        "payload streams arrive unchanged"
    );
    assert!(
        reached_close,
        "the payload reached its out-of-band close marker"
    );
    assert!(
        still_running,
        "the program must still await acknowledgement"
    );
    assert_eq!(
        outcome,
        Some(WaitOutcome::Exited(139)),
        "the report survives"
    );
}

#[test]
fn a_confined_file_releases_its_streams_before_it_exits() {
    assert_program_releases_its_streams(&sh(), &["-c"]);
}

#[cfg(feature = "coreutils")]
#[cfg_attr(
    ctor_without_argv,
    ignore = "a test binary serves bundled tools before main, where musl gives std no argv"
)]
#[test]
fn a_confined_tool_releases_its_streams_before_it_exits() {
    let env = admitted(Program::Tool("env"));
    assert_program_releases_its_streams(&env, &["/bin/sh", "-c"]);
}

/// Where the host cannot build the namespace, the shared table is
/// asserted rather than skipped, so a masked host cannot read as a pass.
#[test]
fn no_host_pid_is_nameable_inside_the_envelope() {
    let restricted = deny_within(&workdir("pidns"), &[]);
    let mut sleeper = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn a host-side sleeper");
    // Nameability is asked of the table, never of `kill -0`: the Landlock
    // signal scope refuses a host pid whether or not it is nameable, so a
    // signal cannot tell the two apart — that is what makes the scope the
    // hole's second closure (`a_confined_child_cannot_signal_a_host_process`).
    let script = format!(
        "[ -d /proc/{me} ] && echo HOST-PID-VISIBLE\n\
         kill -TERM {sleeper} 2>/dev/null && echo SLEEPER-SIGNALLED\n\
         echo PIDS=$(ls /proc | grep -c '^[0-9]')\n\
         echo INIT=$(tr '\\0' ' ' < /proc/1/cmdline)\n",
        me = std::process::id(),
        sleeper = sleeper.id(),
    );
    for policy in [&unrestricted(), &restricted] {
        let Some(env) = envelope_launches(policy) else {
            continue;
        };
        let host = env.host;
        let (stdout, _) = confined(&env, policy, &script);
        if host.private_pids {
            assert!(
                !emitted(&stdout, "HOST-PID-VISIBLE") && !emitted(&stdout, "SLEEPER-SIGNALLED"),
                "a host pid was nameable inside the envelope: {stdout}"
            );
            assert!(
                sleeper.try_wait().expect("poll the sleeper").is_none(),
                "the host-side sleeper was killed from inside the envelope"
            );
            let pids: u32 = field(&stdout, "PIDS=")
                .and_then(|n| n.parse().ok())
                .expect("a pid count");
            assert!(
                pids <= 8,
                "the table inside must be the envelope's own: {stdout}"
            );
            assert!(
                field(&stdout, "INIT=")
                    .and_then(|init| init.split_whitespace().next())
                    .is_some_and(|argv0| {
                        std::path::Path::new(argv0).file_name() == Some("bwrap".as_ref())
                    }),
                "pid 1 inside must be bwrap's init: {stdout}"
            );
        } else {
            assert!(
                emitted(&stdout, "HOST-PID-VISIBLE"),
                "the datum says the table is shared, yet the test's own pid was not nameable: {stdout}"
            );
        }
    }
    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

/// A bind of the host's `/proc` under a pid namespace would make
/// `/proc/self` somebody else's.
#[test]
fn proc_self_is_the_payloads_own_on_every_projection() {
    let restricted = deny_within(&workdir("procself"), &[]);
    let shell = std::fs::canonicalize("/bin/sh").expect("resolve /bin/sh");
    // `read` is a builtin, so `/proc/self` is the shell's; `cat`'s would be its own.
    let script = "echo SELF=$$\n\
                  read pid _ < /proc/self/stat; echo STAT=$pid\n\
                  echo EXE=$(readlink /proc/$$/exe)\n";
    for policy in [&unrestricted(), &restricted] {
        let Some(env) = envelope_launches(policy) else {
            continue;
        };
        let host = env.host;
        let (stdout, _) = confined(&env, policy, script);
        let own: u32 = field(&stdout, "SELF=")
            .and_then(|n| n.parse().ok())
            .expect("the shell's pid");
        assert_eq!(
            field(&stdout, "STAT="),
            Some(own.to_string().as_str()),
            "/proc/self must be the reader's own entry: {stdout}"
        );
        // By name: the restricted envelope binds `/bin` beside `/usr`, so
        // the path is whichever mount the exec went through.
        assert_eq!(
            field(&stdout, "EXE=").and_then(|exe| std::path::Path::new(exe).file_name()),
            shell.file_name(),
            "/proc/<pid>/exe must name the shell's own binary: {stdout}"
        );
        if host.private_pids {
            assert!(
                own < 64,
                "the shell's pid must be namespace-local: {stdout}"
            );
        }
    }
}

#[test]
fn moved_fs_exclusions_are_masked_and_absent_tails_leave_the_directory_visible() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    let secret = tmp.path().join("secret");
    let private = tmp.path().join("private");
    let absent = tmp.path().join("absent");
    for dir in [&work, &private, &absent] {
        std::fs::create_dir(dir).unwrap();
    }
    std::fs::write(&secret, "SECRET-BYTES").unwrap();
    std::fs::write(private.join("key"), "PRIVATE-BYTES").unwrap();
    std::fs::write(work.join("ok"), "ALLOWED-BYTES").unwrap();
    let policy = restricted(&[&work], &[], &[&secret, &private, &absent.join(".env")]);
    let Some(env) = envelope_launches(&restricted(&[&work], &[], &[])) else {
        return;
    };
    std::fs::copy(&secret, work.join("copy")).unwrap();
    std::fs::rename(&secret, work.join("x")).unwrap();
    std::fs::rename(&private, work.join("private")).unwrap();
    std::fs::rename(&absent, work.join("absent")).unwrap();
    let (stdout, stderr) = confined(
        &env,
        &policy,
        &format!(
            "cat '{0}/x' '{0}/private/key' 2>/dev/null; cat '{0}/ok' '{0}/copy'; ls '{0}/absent' && echo TAIL-VISIBLE",
            work.display(),
        ),
    );
    assert!(!stdout.contains("PRIVATE-BYTES"), "{stdout}: {stderr}");
    assert_eq!(
        stdout.matches("SECRET-BYTES").count(),
        1,
        "only the copy is readable: {stdout}: {stderr}"
    );
    assert!(stdout.contains("ALLOWED-BYTES"), "{stdout}: {stderr}");
    assert!(stdout.contains("TAIL-VISIBLE"), "{stdout}: {stderr}");
}
