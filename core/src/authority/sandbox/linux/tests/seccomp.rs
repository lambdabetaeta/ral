use super::*;

// The filter is projection-independent (`seccomp::Filter::ENVELOPE` is
// keyed on nothing in the grant), so the open exec projection is the
// right one to prove it on — these tests are not about exec at all.

fn on_path(tool: &str) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    crate::path::which::locate(tool, Some(&path), None).is_some()
}

/// `RC=159` is `128 + SIGSYS`: an `EPERM` here (`RC=32`, the errno for a
/// missing `CAP_SYS_ADMIN`) would mean the filter never ran and only the
/// lack of capability refused the mount.  Requires `mount`.
#[test]
fn a_mount_inside_the_envelope_is_killed() {
    if !on_path("mount") {
        eprintln!("skipping: no `mount` on this host's PATH");
        return;
    }
    let policy = exec_policy(ExecProjection::Unrestricted);
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let out = run_confined(
        &env,
        &policy,
        "mount -t tmpfs none /tmp 2>/dev/null; echo RC=$?",
    )
    .expect("spawn bwrap");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        field(&stdout, "RC="),
        Some("159"),
        "mount inside the envelope must be killed with SIGSYS, not merely denied: {stdout}"
    );
}

/// A user namespace is refused with `EPERM`, not killed: `unshare`'s own
/// self-check must print "Operation not permitted", and the exit must be
/// neither a clean `0` nor a `159` SIGSYS. Requires `unshare` (util-linux).
#[test]
fn a_user_namespace_is_refused_not_killed() {
    if !on_path("unshare") {
        eprintln!("skipping: no `unshare` on this host's PATH");
        return;
    }
    let policy = exec_policy(ExecProjection::Unrestricted);
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let out = run_confined(&env, &policy, "unshare -U true 2>&1; echo RC=$?").expect("spawn bwrap");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Operation not permitted"),
        "unshare -U must fail with EPERM, worded by unshare itself: {stdout}"
    );
    let rc = field(&stdout, "RC=");
    assert!(
        rc.is_some_and(|rc| rc != "0" && rc != "159"),
        "a user namespace must be refused, neither allowed nor killed: {stdout}"
    );
}

/// `strace` opens with `PTRACE_TRACEME`/`ptrace`, killed with SIGSYS; then
/// `describe_denial` must name it from the very syscall number the kernel
/// logged. Requires `strace`.
#[test]
fn a_confined_ptrace_is_killed_and_named() {
    if !on_path("strace") {
        eprintln!("skipping: no `strace` on this host's PATH");
        return;
    }
    let policy = exec_policy(ExecProjection::Unrestricted);
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let out = run_confined(
        &env,
        &policy,
        "strace -o /dev/null true 2>/dev/null; echo RC=$?",
    )
    .expect("spawn bwrap");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        field(&stdout, "RC="),
        Some("159"),
        "a confined ptrace must be killed with SIGSYS: {stdout}"
    );

    #[cfg(target_arch = "x86_64")]
    let arch = "c000003e";
    #[cfg(target_arch = "aarch64")]
    let arch = "c00000b7";
    let record = crate::authority::sandbox::diag::platform::parse_denial(&format!(
        "type=1326 pid=1 arch={arch} syscall={} code=0x0",
        libc::SYS_ptrace
    ))
    .expect("a kill-thread record names ptrace");
    let described = crate::authority::sandbox::diag::build_hint(&[record]);
    assert!(
        described.contains("ptrace"),
        "the hint must name ptrace from its syscall number and action"
    );
}
