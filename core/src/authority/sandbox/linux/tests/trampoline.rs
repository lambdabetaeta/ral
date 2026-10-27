use super::*;

/// The bind table of a launch whose trampoline is `own`.
fn over(own: &Pinned) -> Vec<String> {
    let bwrap = stand_in();
    let env = Envelope {
        trampoline: own,
        ..on(&bwrap, WHOLE)
    };
    let binds =
        Binds::open(&env, plan(&unrestricted()), &mut Handoff::default()).expect("the binds open");
    (binds.layer(Layer::Over))
        .map(|(_, bind)| bind.dest.as_str().to_string())
        .collect()
}

/// Whether `pin`, unlinked, says so through its descriptor: FUSE (rootless
/// podman's fuse-overlayfs) keeps the link count it cached, so a pin there
/// never loses its name and an unlink cannot be tested on it.
fn unlinked(pin: &Pinned) -> bool {
    let unlinked = pin.names().expect("read the pin").is_none();
    if !unlinked {
        eprintln!(
            "skipping: the filesystem under {} hides an unlink from an open descriptor",
            std::env::temp_dir().display()
        );
    }
    unlinked
}

/// A pin is locked at the name its inode holds now, never the one it was
/// pinned at; unlinked, it has none, and nothing is bound for it.
#[test]
fn a_pin_is_locked_at_its_current_name_and_nowhere_once_unlinked() {
    let dir = workdir("pin-names");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let (pinned_at, moved) = (dir.join("pinned"), dir.join("moved"));
    std::fs::write(&pinned_at, "x").unwrap();
    let pin = Pinned::open(pinned_at.clone()).expect("the file pins");
    std::fs::rename(&pinned_at, &moved).unwrap();
    assert_eq!(pin.names().expect("read the pin"), Some(moved.clone()));
    let locked = over(&pin);
    let name = |p: &std::path::Path| p.to_string_lossy().into_owned();
    assert!(locked.contains(&name(&moved)), "{locked:?}");
    assert!(!locked.contains(&name(&pinned_at)), "{locked:?}");
    std::fs::remove_file(&moved).unwrap();
    if unlinked(&pin) {
        assert_eq!(
            over(&pin),
            over(&stand_in()),
            "an unlinked pin binds nothing"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A copy of this test binary in `dir`, pinned as a trampoline.
fn pinned_copy(dir: &std::path::Path) -> (std::path::PathBuf, Pinned) {
    let copy = dir.join("ral");
    std::fs::copy(std::env::current_exe().expect("own path"), &copy).expect("copy ral");
    let pin = Pinned::open(copy.clone()).expect("the copy pins");
    (copy, pin)
}

/// [`pinned_copy`], swapped as an atomic `cargo install` swaps it: an
/// impostor renamed over its name, the pinned inode left with none.
fn swapped_trampoline(dir: &std::path::Path) -> (std::path::PathBuf, Pinned) {
    use std::os::unix::fs::PermissionsExt;
    let (copy, pin) = pinned_copy(dir);
    let impostor = dir.join("impostor");
    std::fs::write(&impostor, "#!/bin/sh\necho IMPOSTOR\n").unwrap();
    std::fs::set_permissions(&impostor, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&impostor, &copy).unwrap();
    (copy, pin)
}

/// [`run_admitted`] with `own` as the trampoline in place of ral's own
/// pin; the exec projection must be unrestricted, as nothing admits
/// `own`.
fn run_trampoline(
    env: &Envelope<'_>,
    own: &Pinned,
    policy: &SandboxProjection,
    admitted: &Admitted,
    args: &[String],
) -> Option<std::process::Output> {
    use crate::authority::sandbox::warrant::Warrant;
    use std::os::fd::AsFd;
    let parcel = Warrant::new(None, admitted, args, policy)
        .parcel()
        .expect("parcels");
    let mut handoff = Handoff::default();
    let warrant = handoff.lend(parcel.as_fd()).expect("lends");
    let rendered = policy.rendered().expect("renders");
    let env = Envelope {
        trampoline: own,
        ..*env
    };
    let (mut cmd, info_fd) = bwrap_command(
        &env,
        plan(policy),
        handoff,
        warrant,
        &rendered,
        None,
        Ownership::Kept,
    )
    .expect("the launch builds");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd.output().ok();
    drop(info_fd);
    out
}

/// Swapped on disk after its pin, the trampoline still runs the pinned
/// bytes, with no name left to lock: bwrap execs the inode by descriptor,
/// and nothing on Linux re-stats the name to refuse the launch first.
/// Needs a filesystem that execs an unlinked file (M16: not virtiofs).
#[test]
fn a_trampoline_swapped_by_rename_still_runs_the_pinned_bytes() {
    let policy = unrestricted();
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let dir = workdir("trampoline-swap");
    let (_, own) = swapped_trampoline(&dir);
    if unlinked(&own) {
        let out =
            run_trampoline(&env, &own, &policy, &sh(), &sh_args("echo READY")).expect("spawn");
        let (stdout, stderr) = (
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(emitted(&stdout, "READY"), "{stdout} {stderr}");
        assert!(!stdout.contains("IMPOSTOR"), "the name ran: {stdout}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Inside, the trampoline's `/proc/self/exe` is the pinned inode, which
/// after the swap has no name but the one it lost: a bundled tool runs in
/// the trampoline itself.
#[cfg(feature = "coreutils")]
#[cfg_attr(
    ctor_without_argv,
    ignore = "a test binary serves bundled tools before main, where musl gives std no argv"
)]
#[test]
fn the_trampoline_inside_is_the_pinned_inode() {
    let policy = unrestricted();
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let dir = workdir("trampoline-exe");
    let (copy, own) = swapped_trampoline(&dir);
    if unlinked(&own) {
        let readlink = admitted(Program::Tool("readlink"));
        let out = run_trampoline(
            &env,
            &own,
            &policy,
            &readlink,
            &["/proc/self/exe".to_string()],
        )
        .expect("spawn");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim_end(),
            format!("{} (deleted)", copy.display()),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Inside a writable prefix holding it, the trampoline's file is the one
/// name a confined child can neither write, link, rename nor unlink, and
/// the host's bytes are intact after; a sibling stays writable.
#[test]
fn a_pinned_binary_inside_a_writable_prefix_is_locked() {
    let dir = workdir("pin-locked");
    let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
    let policy = deny_within(&dir, &[]);
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let (ral, own) = pinned_copy(&dir);
    let script = format!(
        "cd '{dir}'\n\
         printf x > ral && echo WRITE-DONE\n\
         ln ral linked && echo LINK-DONE\n\
         mv ral moved && echo RENAME-DONE\n\
         rm -f ral && echo UNLINK-DONE\n\
         touch sibling && echo SIBLING-DONE\n",
        dir = dir.display()
    );
    let out = run_trampoline(&env, &own, &policy, &sh(), &sh_args(&script)).expect("spawn");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(emitted(&stdout, "SIBLING-DONE"), "{stdout} {stderr}");
    for refused in ["WRITE-DONE", "LINK-DONE", "RENAME-DONE", "UNLINK-DONE"] {
        assert!(!emitted(&stdout, refused), "{refused}: {stdout} {stderr}");
    }
    let exe = std::env::current_exe().expect("own path");
    assert!(
        std::fs::read(&ral).expect("still there") == std::fs::read(exe).expect("read self"),
        "the pinned bytes changed on the host"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
