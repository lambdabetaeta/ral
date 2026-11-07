use super::landlock::{Abi, Landlock, Landlocked};
use super::{
    Bind, Binds, Builds, CGROUP, Envelope, GpuNodes, HostEnvelope, InfoFd, Layer, Pinned, Shown,
    bwrap_argv, bwrap_command, envelope, open_object, own_cgroup, pin_envelope,
};
use crate::Role;
use crate::authority::policy::{
    Admitted, Baseline, Capabilities, FsAllow, FsOp, FsPolicy, Gpu, Net, Program,
};
use crate::authority::policy::{ExecAllow, ExecDeny, ExecRules, FsDeny, GrantPath, Rule};
use crate::authority::sandbox::launch::{Image, Ownership, admitted, enveloped};
use crate::authority::sandbox::warrant::{Handoff, Slot, Slots};
use crate::authority::sandbox::{ExecProjection, FsLists, FsProjection, SandboxProjection};
use crate::path::{CwdPath, Excluded, FrozenPath, RealPath, Trace};
use crate::process::Unstarted;
use crate::test_helper::confinement::skip;

mod exec;
mod info;
mod pin;
mod render;
mod seccomp;
mod spawn;
mod trampoline;

/// The host's baseline, which a restricted read region carries.
fn host_system_reads() -> Baseline {
    crate::test_helper::core_runtime().baseline().clone()
}

fn dir_rule(path: &RealPath, allow: bool) -> Rule {
    let written = FrozenPath::from_surface(crate::path::utf8(path.as_path()).unwrap());
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

/// `rules` as the kernel holds them, no support beside them.
fn kernel(rules: impl IntoIterator<Item = Rule>) -> ExecProjection {
    ExecProjection::Restricted(rules.into_iter().collect::<ExecRules>().for_kernel())
}

/// A bare Linux host.
const WHOLE: HostEnvelope = HostEnvelope {
    private_pids: true,
    virtual_dev: true,
    private_cgroup: true,
    landlock: Landlock::At(Abi::SCOPES),
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
    exec_policy(ExecProjection::default())
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
    Shown::of(&rendered).expect("the plan builds")
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

/// What a confined launch came to: its output, or why it never started.
type Ran = Result<std::process::Output, Unstarted>;

/// `sh -c script` started as a command under a grant that only withholds
/// the GPU, which needs no Landlock: admitted, prepared and started by the
/// shell, its streams closed, and handed with the shell to `then`.  Callers
/// skip first where no envelope builds.
fn start_granted<R>(
    script: &str,
    then: impl FnOnce(crate::shell::exec::command::Started, &crate::shell::runtime::Shell) -> R,
) -> R {
    use crate::process::{PgidPolicy, StdioSpec};
    use crate::shell::exec::command::{Head, prepare, start};
    let mut shell = crate::test_helper::core_shell();
    let layer = crate::test_helper::layer(Capabilities {
        gpu: Some(crate::authority::policy::Gpu::Withheld),
        ..Capabilities::default()
    });
    shell.with_layer(layer, |shell| {
        let head = Head::resolve(
            &crate::ir::CommandName::Path("/bin/sh".into()),
            &shell.context,
        )
        .admit(&crate::shell::runtime::Mooring::adrift(), shell)
        .expect("the grant admits /bin/sh");
        let args = ["-c", script].map(crate::shell::runtime::Value::string);
        let mut prepared = prepare(head, &args, Ownership::Kept, shell).expect("the launch builds");
        prepared
            .launch
            .stdin(StdioSpec::Null)
            .stdout(StdioSpec::Null)
            .stderr(StdioSpec::Null);
        let started = start(prepared, PgidPolicy::NewLeader, &shell.scope)
            .unwrap_or_else(|broke| panic!("the confined sh must start: {broke:?}"));
        then(started, shell)
    })
}

/// `/bin/sh -c script`, through [`run_admitted`].
fn run_confined(env: &Envelope<'_>, policy: &SandboxProjection, script: &str) -> Ran {
    run_admitted(env, policy, &sh(), &sh_args(script))
}

/// `script` confined, its launch asserted; `(stdout, stderr)`.
#[track_caller]
fn confined(env: &Envelope<'_>, policy: &SandboxProjection, script: &str) -> (String, String) {
    let out = run_confined(env, policy, &format!("echo READY\n{script}")).expect("spawn bwrap");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        emitted(&stdout, "READY"),
        "the envelope did not launch: {stderr}"
    );
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
) -> Ran {
    run_after(env, policy, admitted, args, || ())
}

/// The trampoline's own `execve` of the admitted program was refused, as it
/// reports: a helper's refusal comes from inside a program that started.
fn trampoline_refused(ran: &Ran) -> bool {
    matches!(
        ran,
        Err(Unstarted::Program(e)) if e.kind() == std::io::ErrorKind::PermissionDenied
    )
}

/// [`run_admitted`], built now and spawned after `between`: the window in
/// which a same-uid writer races the launch.
fn run_after(
    env: &Envelope<'_>,
    policy: &SandboxProjection,
    admitted: &Admitted,
    args: &[String],
    between: impl FnOnce(),
) -> Ran {
    let mut launch = enveloped(
        env,
        policy,
        &Image::of(admitted),
        args,
        None,
        Ownership::Kept,
    )
    .expect("the launch builds");
    between();
    launch.stdin(crate::process::StdioSpec::Null);
    launch.output()
}

/// This host's pinned bwrap running ral's own pin, where it can build an
/// envelope at all.  Where it cannot — bwrap absent, or user namespaces
/// unavailable — a spawning test proves nothing either way: it says so and
/// stops, or fails where the sandbox is required.
fn envelope_launches(policy: &SandboxProjection) -> Option<Envelope<'static>> {
    pin_envelope();
    let bwrap = match envelope() {
        Ok(bwrap) => bwrap,
        Err(why) => {
            skip(why);
            return None;
        }
    };
    let own = crate::authority::sandbox::reexec::own().expect("ral pins itself");
    let env = Envelope::probe(bwrap, own);
    let control = run_confined(&env, policy, "echo READY");
    if control
        .as_ref()
        .is_ok_and(|o| emitted(&String::from_utf8_lossy(&o.stdout), "READY"))
    {
        return Some(env);
    }
    let why = match control {
        Ok(o) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
        Err(Unstarted::Refused(refusal)) => refusal.to_string(),
        Err(Unstarted::Program(e)) => format!("the trampoline could not start /bin/sh: {e}"),
        Err(Unstarted::Stopped(stop)) => format!("the control was stopped: {stop:?}"),
    };
    skip(format_args!(
        "this host cannot build a bwrap envelope: {why}"
    ));
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
        net: None,
        gpu: None,
        exec,
    }
}
