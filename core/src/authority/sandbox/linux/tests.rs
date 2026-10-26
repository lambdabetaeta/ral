use super::{
    Access, Binds, Builds, CGROUP, Envelope, GpuNodes, HostEnvelope, Layer, Pinned, Shown,
    bwrap_argv, bwrap_command,
};
use crate::Role;
use crate::authority::policy::{Admitted, Baseline, Capabilities, FsAllow, FsPolicy, Program};
use crate::authority::policy::{ExecAllow, ExecDeny, GrantPath, Join, Rule};
use crate::authority::sandbox::launch::{Ownership, admitted, enveloped};
use crate::authority::sandbox::warrant::{Handoff, Slot};
use crate::authority::sandbox::{ExecProjection, FsProjection, FsRules, SandboxProjection};
use crate::path::{Excluded, FrozenPath, RealPath, Trace};
use std::process::Stdio;

mod exec;
mod render;
mod seccomp;
mod spawn;
mod trampoline;

/// The host's baseline, which a restricted read region carries.
fn host_system_reads() -> Baseline {
    crate::test_helper::core_runtime().baseline().clone()
}

fn dir_rule(path: &RealPath, allow: bool) -> Rule {
    let written = FrozenPath::of_real(path);
    if allow {
        Rule::Allow(ExecAllow::Dir(
            GrantPath::<Trace>::walk(written).expect("a directory walks"),
        ))
    } else {
        Rule::Deny(Box::new(ExecDeny::Dir(
            GrantPath::<Excluded>::walk(written).expect("a directory walks"),
        )))
    }
}

/// A bare Linux host.
const WHOLE: HostEnvelope = HostEnvelope {
    private_pids: true,
    virtual_dev: true,
    private_cgroup: true,
    landlock: super::landlock::Landlock::At(super::landlock::Abi::SCOPES),
    builds: Builds::ByFd,
};

fn workdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ral-bwrap-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create work dir");
    dir
}

fn deny_within(dir: &std::path::Path, denied: &[&std::path::Path]) -> SandboxProjection {
    restricted(&[], &[dir], denied)
}

fn unrestricted() -> SandboxProjection {
    SandboxProjection {
        fs: FsProjection::Unrestricted,
        net: true,
        gpu: true,
        exec: ExecProjection::default(),
    }
}

/// This test binary stands in for bwrap and the trampoline where only the
/// options are read: the render tests need no bwrap on the host, and no
/// payload shares its name.
fn stand_in() -> Pinned {
    Pinned::open(std::env::current_exe().expect("own path")).expect("own binary pins")
}

/// `pin` as both bwrap and the trampoline, on `host`.
fn on(pin: &Pinned, host: HostEnvelope) -> Envelope<'_> {
    Envelope {
        bwrap: pin,
        trampoline: pin,
        host,
    }
}

/// What the envelope would show of `policy`.
fn plan(policy: &SandboxProjection) -> Shown {
    let rendered = policy.rendered().expect("ASCII paths render");
    Shown::of(&rendered, &host_system_reads()).expect("the plan builds")
}

/// `real/tool`, runnable, under a fresh work dir with `linkdir -> real`
/// beside it: the dir, and the tool's real path.
fn linked_tool(tag: &str) -> (std::path::PathBuf, String) {
    use std::os::unix::fs::PermissionsExt;
    let dir = workdir(tag);
    let tool = dir.join("real/tool");
    std::fs::create_dir_all(dir.join("real")).unwrap();
    std::fs::write(&tool, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(dir.join("real"), dir.join("linkdir")).unwrap();
    let tool = std::fs::canonicalize(&tool).unwrap();
    (dir, tool.to_string_lossy().into_owned())
}

/// Through the fold, as a launch projects a grant: the baseline included.
fn restricted(
    read: &[&std::path::Path],
    write: &[&std::path::Path],
    deny: &[&std::path::Path],
) -> SandboxProjection {
    let frozen = |paths: &[&std::path::Path]| {
        paths
            .iter()
            .map(|p| FrozenPath::from_surface(crate::path::utf8(p).unwrap()))
            .collect()
    };
    let fs = FsPolicy {
        read_prefixes: frozen(read),
        write_prefixes: frozen(write),
        deny_paths: frozen(deny),
    };
    let caps = Capabilities {
        fs: Some(fs),
        ..Capabilities::default()
    };
    let stack = crate::test_helper::stack([crate::test_helper::layer(caps)]);
    SandboxProjection::of(&stack, &host_system_reads(), None).expect("an fs grant projects")
}

/// `/bin/sh -c script`, through [`run_admitted`].
fn run_confined(
    env: &Envelope<'_>,
    policy: &SandboxProjection,
    script: &str,
) -> Option<std::process::Output> {
    run_admitted(env, policy, &sh(), &sh_args(script))
}

/// `script` confined, its launch asserted; `(stdout, stderr)`.
fn confined(env: &Envelope<'_>, policy: &SandboxProjection, script: &str) -> (String, String) {
    confined_with("the envelope did not launch", env, policy, script)
}

/// [`confined`], a failure to launch prefixed by `label`.
fn confined_as(
    label: &str,
    env: &Envelope<'_>,
    policy: &SandboxProjection,
    script: &str,
) -> (String, String) {
    let failure = format!("{label}: the envelope did not launch");
    confined_with(&failure, env, policy, script)
}

fn confined_with(
    failure: &str,
    env: &Envelope<'_>,
    policy: &SandboxProjection,
    script: &str,
) -> (String, String) {
    let out = run_confined(env, policy, &format!("echo READY\n{script}")).expect("spawn bwrap");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(emitted(&stdout, "READY"), "{failure}: {stderr}");
    (stdout, stderr)
}

fn sh() -> Admitted {
    admitted(Program::file("/bin/sh".into()).expect("/bin/sh exists"))
}

fn sh_args(script: &str) -> [String; 2] {
    ["-c".to_string(), script.to_string()]
}

/// Through the very argv a launch builds: the payload is the trampoline,
/// which enters the Landlock layer before `execve`ing the admitted
/// program.  A broken trampoline is exactly what these tests exist to
/// catch, so even the positive control goes this way.
fn run_admitted(
    env: &Envelope<'_>,
    policy: &SandboxProjection,
    admitted: &Admitted,
    args: &[String],
) -> Option<std::process::Output> {
    run_after(env, policy, admitted, args, || ())
}

/// [`run_admitted`], built now and spawned after `between`: the window in
/// which a same-uid writer races the launch.
fn run_after(
    env: &Envelope<'_>,
    policy: &SandboxProjection,
    admitted: &Admitted,
    args: &[String],
    between: impl FnOnce(),
) -> Option<std::process::Output> {
    let (mut cmd, info_fd) = enveloped(
        env,
        policy,
        &host_system_reads(),
        admitted,
        args,
        None,
        Ownership::Kept,
    )
    .expect("the launch builds");
    between();
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd.output().ok();
    // Kept open across the fork inside `output()`, whose hook names its copies' numbers.
    drop(info_fd);
    out
}

/// This host's pinned bwrap running ral's own pin, where it can build an
/// envelope at all.  Where it cannot — bwrap absent, or user namespaces
/// unavailable — a spawning test proves nothing either way and says so on
/// the way out.
fn envelope_launches(policy: &SandboxProjection) -> Option<Envelope<'static>> {
    super::pin_envelope();
    let Ok(bwrap) = super::envelope() else {
        eprintln!("skipping: this host has no bwrap to pin");
        return None;
    };
    let own = crate::authority::sandbox::reexec::own().expect("ral pins itself");
    let env = Envelope::probe(bwrap, own);
    let control = run_confined(&env, policy, "echo READY");
    if control
        .as_ref()
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains("READY"))
    {
        return Some(env);
    }
    let why = control.map_or_else(
        || "the envelope did not spawn".to_string(),
        |o| String::from_utf8_lossy(&o.stderr).trim().to_string(),
    );
    eprintln!("skipping: this host cannot build a bwrap envelope: {why}");
    None
}

fn field<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
    stdout.lines().find_map(|line| line.strip_prefix(key))
}

/// A whole line: the script's own text echoes back in `/proc/1/cmdline`.
fn emitted(stdout: &str, marker: &str) -> bool {
    stdout.lines().any(|line| line == marker)
}

/// `exec` as the sole axis under test.  The fs stays open on purpose: a
/// prefix that also hid the planted binary would let these tests pass on a
/// layer that confines nothing.
fn exec_policy(exec: ExecProjection) -> SandboxProjection {
    SandboxProjection {
        fs: FsProjection::Unrestricted,
        net: true,
        gpu: true,
        exec,
    }
}
