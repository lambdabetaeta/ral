use super::*;

/// `sh -c script` confined under `unrestricted()` through the path a
/// real command takes, `Launch::spawn` then `RunningChild`, and the
/// group the spawn named.  Callers skip first where no envelope builds.
fn spawn_confined(
    script: &str,
    scope: &crate::process::Scope,
) -> (
    crate::shell::exec::command::RunningChild,
    crate::process::Pgid,
) {
    use crate::process::{Group, PgidPolicy};
    use crate::shell::exec::command::{Pumps, RunningChild};
    let shell = crate::test_helper::core_shell();
    let mut launch = crate::authority::sandbox::sandboxed_command(
        &unrestricted(),
        &Baseline::default(),
        &sh(),
        &sh_args(script),
        Ownership::Kept,
        shell.context.cwd(),
        scope,
    )
    .expect("build confined launch");
    let (child, pgid, jail) = launch
        .spawn(PgidPolicy::NewLeader)
        .expect("spawn confined sh");
    let pgid = pgid.expect("a kept launch names its payload's group");
    let running = RunningChild::assemble_with_owner(
        child,
        "sh".to_string(),
        Pumps::default(),
        Some(Group::Owns(pgid)),
        scope.clone(),
        jail,
    );
    (running, pgid)
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
    let out = run_admitted(&env, &apart, &admitted(tool()), &[]).expect("spawn bwrap");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("READY")
            && String::from_utf8_lossy(&out.stderr).contains(&real),
        "an image no read shows ran: {out:?}"
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
    let out = run_after(&env, &policy, &sh(), &sh_args("echo READY"), || {
        std::fs::rename(&inner, outer.join("moved")).unwrap();
        std::os::unix::fs::symlink("moved", &inner).unwrap();
    })
    .expect("spawn bwrap");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("READY"),
        "a mount followed a symlinked destination: {}",
        String::from_utf8_lossy(&out.stderr)
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
         : > /dev/shm/probe && echo SHM-OK\n\
         exec 3<>/dev/ptmx && echo PTMX-OK\n",
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
    // The pty is the host devpts's, opened at its own `ptmx`, which that
    // devpts's `ptmxmode` may close: the envelope serves one exactly where
    // the host's devpts does.
    let host_serves = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/pts/ptmx")
        .is_ok();
    assert_eq!(
        stdout.contains("PTMX-OK"),
        host_serves,
        "the by-hand /dev/ptmx must be the host devpts's own: {stdout} {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A host terminal: both ends held open, and the slave's name.
struct HostPty {
    _ends: [std::os::fd::OwnedFd; 2],
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
                _ends: [OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)],
                name: std::ffi::CStr::from_ptr(name.as_ptr())
                    .to_str()
                    .unwrap()
                    .to_owned(),
            }
        }
    }
}

/// A terminal open at render is masked; one the host opens after it, the
/// declared residual, is reachable, and so is the control.
#[test]
fn the_by_hand_dev_hides_host_terminals_and_allocates_its_own() {
    use crate::test_helper::confinement::{Output, admitted as accepts, denied, probe};
    use std::os::unix::fs::PermissionsExt;
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
        let (mut command, info) = enveloped(
            &env,
            &policy,
            &host_system_reads(),
            &program,
            &[],
            None,
            Ownership::Kept,
        )
        .unwrap();
        let late = HostPty::open();
        let args = args(&late.name);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = command
            .env("RAL_TEST_PROBE", probe::request(&args))
            .output()
            .unwrap();
        drop(info);
        out.into()
    };
    let control = invoke(&|late| vec!["pty".into(), late.into()]);
    let hidden = invoke(&|_| vec!["pty".into(), early.name.clone()]);
    denied(&proof, &control, &hidden, "ok");
    assert!(hidden.stderr.contains("Permission denied"), "{hidden:?}");
    let ptmx = std::fs::metadata("/dev/pts/ptmx").expect("the host devpts has a ptmx node");
    if ptmx.permissions().mode() & 0o666 == 0 {
        eprintln!(
            "skip the_by_hand_dev_hides_host_terminals_and_allocates_its_own, allocation only: \
             the host devpts has ptmxmode=000, so the by-hand /dev allocates no pty"
        );
        return;
    }
    accepts(&proof, &invoke(&|_| vec!["pty".into()]), "ok");
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
/// `Launch` + `RunningChild` path a real command takes, so the `--info-fd`
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
    let script = format!(
        "trap 'echo GRACE > {} ; exit 0' TERM\nsleep 30\n",
        flag.display()
    );

    let scope = DurableRoot::default().worker();
    let (running, _) = spawn_confined(&script, &scope);

    let striker = scope.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        striker.cancel(CancelCause::Cancelled);
    });

    let t0 = std::time::Instant::now();
    let waited = running.wait();
    let elapsed = t0.elapsed();
    let (outcome, cause) = (waited.outcome, waited.cause);
    waited.settle(&scope);
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
    let scope = DurableRoot::default().worker();
    let (running, pgid) = spawn_confined("sleep 30", &scope);
    assert!(
        rustix::process::test_kill_process_group(pgid.as_pid()).is_ok(),
        "the payload's group {pgid} must exist once its spawn returns"
    );

    scope.cancel(CancelCause::Aborted);
    let t0 = std::time::Instant::now();
    let waited = running.wait();
    let elapsed = t0.elapsed();
    let outcome = waited.outcome.enveloped(true);
    waited.settle(&scope);
    assert!(
        elapsed.as_secs() < 5,
        "an abort's one kill must end the payload, not wait out its sleep: took {elapsed:?}"
    );
    assert_eq!(
        outcome,
        crate::process::WaitOutcome::Enveloped(128 + libc::SIGKILL),
        "the payload must die of the abort's kill"
    );
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
