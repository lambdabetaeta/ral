//! Per-command OS sandbox launching.
//!
//! This module confines a single external/bundled child: it takes the
//! already-folded effective [`SandboxProjection`] and builds a `Command`
//! that runs one target program under the platform backend.  The command
//! runner's `build_command` routes through here whenever a projection is
//! active (see `decisions/260617_sandbox-external-children`).
//!
//! Env/cwd and resource limits are deliberately **not** applied here. The
//! caller layers `apply_env` + `apply_resource_limits` on the returned
//! `Command` uniformly, exactly as it does for the unsandboxed path, so
//! the two paths cannot drift on how a child inherits cwd/PWD/overrides.
//! Linux is the one place the in-sandbox cwd needs explicit help: bwrap
//! does not honour the launcher's `current_dir`, so the target's logical
//! cwd is threaded into the bwrap argv as `--chdir <cwd>` from `shell`.

use crate::types::{Break, Error, Settled, Shell};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::Command;

/// What [`sandboxed_command`] should run under the OS sandbox.  A host
/// program is its resolved path/bare name; a bundled tool is run as a
/// child placement of ral itself (`ral --ral-bundled-tool <tool> …`).
#[derive(Clone, Copy)]
pub(crate) enum LaunchTarget<'a> {
    Host { program: &'a str },
    BundledTool { tool: &'a str },
}

/// Whether the session keeps owning what it launches — the one thing the
/// *envelope's* lifetime turns on.
///
/// A [`Kept`](Self::Kept) child is one this process waits on and can
/// signal, so its confinement is built to die with us: bwrap gets
/// `--die-with-parent`, and an envelope orphaned by a crash cannot outlive
/// the session that authorised it.  A `Surrendered`
/// one is a `detach` (SPEC §13.7), where that flag would kill the survivor
/// moments after birth and hand back a receipt for nothing.  Dropping it
/// there is not a hole: the confinement still applies for the survivor's
/// whole life, frozen as the frame that birthed it left it, and no later
/// frame can widen what a process nothing can name is allowed to do.
///
/// Read only by the Linux backend.  macOS enters Seatbelt in-process and
/// `execve`s the target in place, so there is no envelope process to tie
/// to a parent, and Windows has no `detach` at all — which is why
/// surrender is a distinction only where the verb that makes it exists.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ownership {
    Kept,
    #[cfg(unix)]
    Surrendered,
}

/// Build a [`Launch`](crate::process::Launch) that runs `target` + `args`
/// under the OS sandbox for `projection`.  The caller has already confirmed
/// [`super::projection_enforceable`].  Env/cwd and resource limits are
/// applied by the caller afterwards, not here.
///
/// Linux and macOS build a `std::process::Command` (bwrap argv / a Seatbelt
/// self re-exec) and adopt it via [`Launch::from_command`](crate::process::Launch::from_command);
/// Windows builds the target `Launch` directly and attaches the `AppContainer`
/// `SECURITY_CAPABILITIES` the parent's spawn applies (no re-exec).
///
/// `shell` is taken only for the target's logical cwd, which Linux folds
/// into the bwrap argv as `--chdir`; `ownership` likewise reaches only the
/// Linux backend, which alone builds an envelope process to tie to us.
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        unused_variables,
        reason = "shell.cwd() and ownership are consumed by the bwrap argv alone"
    )
)]
pub(crate) fn sandboxed_command(
    projection: &crate::types::SandboxProjection,
    target: LaunchTarget,
    args: &[String],
    ownership: Ownership,
    shell: &Shell,
) -> Settled<crate::process::Launch> {
    #[cfg(target_os = "linux")]
    {
        linux_sandboxed_command(projection, target, args, ownership, shell)
            .map(crate::process::Launch::from_command)
    }
    #[cfg(target_os = "macos")]
    {
        macos_sandboxed_command(projection, target, args).map(crate::process::Launch::from_command)
    }
    #[cfg(windows)]
    {
        windows_sandboxed_command(projection, target, args)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (projection, target, args);
        Err(Break::Error(Error::new(
            "no per-command sandbox backend on this platform",
            1,
        )))
    }
}

/// Windows: build the target's [`Launch`](crate::process::Launch) directly
/// and confine it under the session `AppContainer`.
///
/// There is no re-exec into the sandbox as on macOS/Linux — the parent
/// applies the `LowBox` token at spawn time via
/// [`Launch::security_capabilities`](crate::process::Launch::security_capabilities)
/// (wired in [`super::windows::session::confine`]).  A
/// [`LaunchTarget::BundledTool`] is therefore a plain
/// `ral --ral-bundled-tool <tool> …` self-placement that runs confined under
/// that token, carrying no `--sandbox-projection`: its own `early_init` has
/// nothing to enter, matching [`super::reexec::maybe_enter_process_sandbox`].
#[cfg(windows)]
fn windows_sandboxed_command(
    projection: &crate::types::SandboxProjection,
    target: LaunchTarget,
    args: &[String],
) -> Settled<crate::process::Launch> {
    // The AppContainer child must be able to read+execute its program image;
    // a user-installed binary is not readable by the LowBox token otherwise
    // (only the ALL APPLICATION PACKAGES system paths are).  `session::confine`
    // stamps this path RO, mirroring the Linux backend binding the program
    // path RO into the bwrap argv.
    let (mut launch, image): (crate::process::Launch, Option<std::path::PathBuf>) = match target {
        LaunchTarget::Host { program } => {
            let mut launch = crate::process::Launch::new(program);
            launch.args(args);
            // Only an absolute program path can be granted here: a bare name
            // resolves on PATH by the loader (System32 tools are ALL
            // APPLICATION PACKAGES-readable), and a name we have not resolved
            // is not a path we can stamp — so a bare-name image's readability
            // rests on the fs read projection / AAP, not on an image grant.
            let image =
                crate::path::is_absolute(program).then(|| std::path::PathBuf::from(program));
            (launch, image)
        }
        LaunchTarget::BundledTool { tool } => {
            use crate::runtime::pipeline::helper::{BUNDLED_TOOL_FLAG, self_reexec};
            let mut launch = self_reexec(BUNDLED_TOOL_FLAG).map_err(|e| {
                Break::Error(Error::new(
                    format!("sandbox: cannot resolve self exe for bundled tool '{tool}': {e}"),
                    1,
                ))
            })?;
            launch.arg(tool);
            launch.args(args);
            // The confined child is ral.exe itself; grant it RO so the token
            // can load the image the `--ral-bundled-tool` self-reexec targets.
            let image = super::reexec::self_exec_path();
            (launch, image)
        }
    };
    super::windows::session::confine(&mut launch, projection, image.as_deref())?;
    Ok(launch)
}

/// Linux: expand the target into the bwrap argv via
/// [`super::linux::make_command_with_policy`], which binds an absolute
/// program path RO so bwrap can exec it.  A [`LaunchTarget::BundledTool`]
/// resolves to the on-disk self exe so bwrap re-execs *us* with
/// `--ral-bundled-tool <tool> …`.
///
/// The self-path used here is the on-disk `arg0` (`current_exe()`), not
/// the fd-pinned `exec_path` (`/proc/self/fd/N`): bwrap mounts a fresh
/// `/proc`, so a `/proc/self/fd` target would neither bind nor resolve in
/// the child.  This matches `respawn_under_bwrap`, which likewise hands
/// bwrap `current_exe()`.
///
/// The target's logical cwd is threaded in as bwrap's `--chdir`: the
/// caller's `apply_env` sets the launcher (bwrap) process cwd, but bwrap
/// runs the child in its mount-namespace root by default, so the
/// in-sandbox cwd must be requested explicitly.
#[cfg(target_os = "linux")]
fn linux_sandboxed_command(
    projection: &crate::types::SandboxProjection,
    target: LaunchTarget,
    args: &[String],
    ownership: Ownership,
    shell: &Shell,
) -> Settled<Command> {
    let cwd = shell.cwd().to_string_lossy().into_owned();
    match target {
        LaunchTarget::Host { program } => Ok(super::linux::make_command_with_policy(
            program,
            args,
            projection,
            Some(cwd.as_str()),
            ownership,
        )),
        LaunchTarget::BundledTool { tool } => {
            let self_path = super::reexec::self_arg0().map_err(|e| {
                Break::Error(Error::new(
                    format!("sandbox: cannot resolve self exe for bundled tool: {e}"),
                    1,
                ))
            })?;
            let mut tool_args = Vec::with_capacity(args.len() + 2);
            tool_args.push(crate::runtime::pipeline::helper::BUNDLED_TOOL_FLAG.to_string());
            tool_args.push(tool.to_string());
            tool_args.extend_from_slice(args);
            Ok(super::linux::make_command_with_policy(
                &self_path.to_string_lossy(),
                &tool_args,
                projection,
                Some(cwd.as_str()),
                ownership,
            ))
        }
    }
}

/// macOS: re-exec ral with the folded projection so the child enters
/// Seatbelt in `early_init`, then runs the target *inside* that Seatbelt.
///
/// - [`LaunchTarget::BundledTool`] uses the existing `--ral-bundled-tool`
///   tail: after `early_init` enters Seatbelt, `try_run_bundled_tool`
///   runs the tool in-process, confined — no further exec needed.
/// - [`LaunchTarget::Host`] uses the [`super::SANDBOX_EXEC_FLAG`] tail:
///   after Seatbelt is entered, [`super::serve_sandbox_exec`] `execve`s
///   the host program inside the Seatbelt this process just entered.
#[cfg(target_os = "macos")]
fn macos_sandboxed_command(
    projection: &crate::types::SandboxProjection,
    target: LaunchTarget,
    args: &[String],
) -> Settled<Command> {
    let json = serde_json::to_string(projection).map_err(|e| {
        Break::Error(Error::new(
            format!("sandbox: failed to encode projection: {e}"),
            1,
        ))
    })?;
    // Preserve binary-swap protection: refuse to re-exec if our pinned
    // executable was swapped on disk since boot (e.g. a mid-session
    // `cargo install`), which would otherwise launch a foreign build.
    if let Some(s) = super::reexec::SANDBOX_SELF.get() {
        super::reexec::verify_unswapped(s).map_err(Break::Error)?;
    }
    let mut cmd = super::self_command().map_err(|e| {
        Break::Error(Error::new(
            format!("sandbox: failed to pin self for re-exec: {e}"),
            1,
        ))
    })?;
    cmd.arg(super::SANDBOX_PROJECTION_FLAG).arg(json);
    match target {
        LaunchTarget::BundledTool { tool } => {
            cmd.arg(crate::runtime::pipeline::helper::BUNDLED_TOOL_FLAG)
                .arg(tool)
                .args(args);
        }
        LaunchTarget::Host { program } => {
            cmd.arg(super::SANDBOX_EXEC_FLAG).arg(program).args(args);
        }
    }
    Ok(cmd)
}

/// macOS hook: after `early_init` has entered Seatbelt, run the host
/// program of a `LaunchTarget::Host` re-exec carried in the
/// `--ral-sandbox-exec` tail.
///
/// `execve` replaces this process, so on
/// success it never returns; a spawn failure surfaces as 127, the POSIX
/// "command not found / cannot exec" code.
///
/// `args` is the post-`early_init` argv (Seatbelt already applied).  When
/// it does not start with the sentinel this returns `None` and the normal
/// dispatch continues.
#[cfg(target_os = "macos")]
pub fn serve_sandbox_exec(args: &[String]) -> Option<u8> {
    use std::os::unix::process::CommandExt;

    let (flag, rest) = args.split_first()?;
    if flag != super::SANDBOX_EXEC_FLAG {
        return None;
    }
    let Some((program, prog_args)) = rest.split_first() else {
        let flag = super::SANDBOX_EXEC_FLAG;
        crate::diagnostic::cmd_error("ral", &format!("{flag} requires a program"));
        return Some(127);
    };
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:respawn-spawn] sandbox respawn handoff: builds the Command for the confined re-exec; the surface card fired before this handoff, so the exec itself raises no card."
    )]
    let mut cmd = Command::new(program);
    cmd.args(prog_args);
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:respawn-exec] sandbox respawn handoff: `exec` replaces this process image with the confined target; the surface card fired before this handoff, so the exec itself raises no card."
    )]
    let err = cmd.exec();
    crate::diagnostic::cmd_error("ral", &format!("{program}: {err}"));
    Some(127)
}

/// Off-macOS the `--ral-sandbox-exec` tail is never emitted, so the hook
/// is a no-op that always declines.
///
/// Linux expands the host target into the bwrap argv directly; Windows
/// confines at the parent's spawn via the `LowBox` token, with no child
/// re-entry.
#[cfg(not(target_os = "macos"))]
pub fn serve_sandbox_exec(_args: &[String]) -> Option<u8> {
    None
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    // Used only by the per-platform argv-shape tests (Linux / macOS); on
    // other targets the only test is the sentinel-decline check, which
    // needs neither helper.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use crate::types::{FsPolicy, FsProjection, SandboxProjection};

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn restrictive() -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Restricted(FsPolicy {
                read_prefixes: vec![crate::path::NormalizedPrefix::from_surface("/tmp")],
                write_prefixes: vec![crate::path::NormalizedPrefix::from_surface("/tmp")],
                deny_paths: vec![crate::path::NormalizedPrefix::from_surface("/etc")],
            }),
            net: true,
            exec: crate::types::ExecProjection::default(),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn argv(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// macOS renders a host launch as a self re-exec carrying the
    /// projection and the `--ral-sandbox-exec` tail: the child enters
    /// Seatbelt in `early_init`, then `execve`s the host program inside it.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_host_command_is_self_reexec_with_sandbox_exec_tail() {
        let projection = restrictive();
        let cmd = macos_sandboxed_command(
            &projection,
            LaunchTarget::Host { program: "/bin/sh" },
            &["-c".into(), "echo x > /etc/ral_denied".into()],
        )
        .expect("build macOS host command");
        let args = argv(&cmd);
        // ral --sandbox-projection <json> --ral-sandbox-exec /bin/sh -c …
        assert_eq!(args[0], super::super::SANDBOX_PROJECTION_FLAG);
        let decoded: SandboxProjection =
            serde_json::from_str(&args[1]).expect("projection round-trips");
        assert_eq!(decoded, projection);
        assert_eq!(args[2], super::super::SANDBOX_EXEC_FLAG);
        assert_eq!(args[3], "/bin/sh");
        assert_eq!(&args[4..], ["-c", "echo x > /etc/ral_denied"]);
    }

    /// macOS renders a bundled-tool launch with the existing
    /// `--ral-bundled-tool` tail (no `--ral-sandbox-exec`): the tool runs
    /// in-process inside Seatbelt, no further exec needed.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_bundled_tool_command_uses_bundled_tool_tail() {
        let cmd = macos_sandboxed_command(
            &restrictive(),
            LaunchTarget::BundledTool { tool: "ls" },
            &["-l".into()],
        )
        .expect("build macOS bundled command");
        let args = argv(&cmd);
        assert_eq!(args[0], super::super::SANDBOX_PROJECTION_FLAG);
        assert_eq!(args[2], crate::runtime::pipeline::helper::BUNDLED_TOOL_FLAG);
        assert_eq!(args[3], "ls");
        assert_eq!(args[4], "-l");
        assert!(
            !args.iter().any(|a| a == super::super::SANDBOX_EXEC_FLAG),
            "bundled tool must not carry the host-exec sentinel"
        );
    }

    /// `serve_sandbox_exec` declines argv that does not open with the
    /// sentinel, so the normal post-`early_init` dispatch continues.
    #[test]
    fn serve_sandbox_exec_declines_without_sentinel() {
        assert_eq!(serve_sandbox_exec(&["echo".into(), "hi".into()]), None);
    }

    /// A `SandboxProjection` whose only writable region is `write_dir`,
    /// readable everywhere `/bin/sh` and bundled tools need (the baseline
    /// `system_paths` reads under `/bin`, `/usr`, `/System`, `/lib`,
    /// `/private/etc`, … are emitted unconditionally by the macOS profile,
    /// so the grant need not re-list them) plus `write_dir` for read.
    /// `net`/`exec` are left wide so the *only* thing the profile denies is
    /// a write outside `write_dir` — the test then fails for that reason
    /// alone, not because the interpreter could not start.
    #[cfg(target_os = "macos")]
    fn write_confined_to(write_dir: &str) -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Restricted(FsPolicy {
                read_prefixes: vec![crate::path::NormalizedPrefix::from_surface(write_dir)],
                write_prefixes: vec![crate::path::NormalizedPrefix::from_surface(write_dir)],
                deny_paths: vec![],
            }),
            net: true,
            exec: crate::types::ExecProjection::default(),
        }
    }

    /// A process-unique temp directory under the system temp root, created
    /// on the host (outside any sandbox) so the confined child can write
    /// *into* it.  `std::process::id` keeps it unique without `Date.now` /
    /// randomness, per the deterministic-temp rule.
    #[cfg(target_os = "macos")]
    fn unique_workdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ral_launch_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create work dir");
        dir
    }

    /// Spawn `cmd` confined, wait, and report whether the child exited
    /// successfully.  Stdio is silenced so a denied write's diagnostic does
    /// not pollute the test runner's output.
    #[cfg(target_os = "macos")]
    fn run_confined(mut cmd: Command) -> bool {
        use std::process::Stdio;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().expect("spawn sandboxed child");
        // A one-shot sandboxed test child that cannot be SIGSTOP'd by user
        // code; the WUNTRACED dance of ChildHandle would be pointless here,
        // matching `linux::respawn_under_bwrap` and the pipeline anchor.
        #[allow(clippy::disallowed_methods)]
        let status = child.wait().expect("wait for sandboxed child");
        status.success()
    }

    /// End-to-end per-command denial through the host seam on macOS: a
    /// `LaunchTarget::Host { /bin/sh }` confined to `work` may write a file
    /// *inside* `work` (positive control) but is denied a write *outside*
    /// it — proving Seatbelt enforces the projection rather than failing
    /// blanket.  `/bin/sh` needs no extra read grants: the profile's
    /// baseline system reads cover its dylibs and `/bin/sh` itself.
    #[cfg(target_os = "macos")]
    #[test]
    fn external_denied_write_outside_fs_grant() {
        let work = unique_workdir("ext");
        let work_s = work.to_string_lossy().into_owned();
        let proj = write_confined_to(&work_s);

        // Positive control: a write inside the granted prefix succeeds.
        let allowed = work.join("allowed.txt");
        let allowed_s = allowed.to_string_lossy().into_owned();
        let ok = macos_sandboxed_command(
            &proj,
            LaunchTarget::Host { program: "/bin/sh" },
            &["-c".into(), format!("echo x > {allowed_s}")],
        )
        .expect("build host command (inside)");
        assert!(run_confined(ok), "write into the write prefix must succeed");
        assert!(allowed.exists(), "in-prefix write should have landed");

        // Denial: a write outside the granted prefix fails, and the file
        // never appears.
        let denied = std::env::temp_dir().join(format!("ral_denied_ext_{}", std::process::id()));
        let _ = std::fs::remove_file(&denied);
        let denied_s = denied.to_string_lossy().into_owned();
        let bad = macos_sandboxed_command(
            &proj,
            LaunchTarget::Host { program: "/bin/sh" },
            &["-c".into(), format!("echo x > {denied_s}")],
        )
        .expect("build host command (outside)");
        assert!(
            !run_confined(bad),
            "write outside the write prefix must fail"
        );
        assert!(
            !denied.exists(),
            "out-of-prefix write must not have landed at {denied_s}"
        );

        let _ = std::fs::remove_dir_all(&work);
        let _ = std::fs::remove_file(&denied);
    }

    /// Same enforcement proof through the bundled-tool seam: `mkdir` run as
    /// a confined `--ral-bundled-tool` child creates a directory inside the
    /// write prefix (positive control) but is denied one outside it.
    #[cfg(all(target_os = "macos", feature = "coreutils"))]
    #[test]
    fn bundled_tool_denied_outside_fs_grant() {
        let work = unique_workdir("bun");
        let work_s = work.to_string_lossy().into_owned();
        let proj = write_confined_to(&work_s);

        // Positive control: mkdir inside the granted prefix succeeds.
        let allowed = work.join("sub");
        let allowed_s = allowed.to_string_lossy().into_owned();
        let ok = macos_sandboxed_command(
            &proj,
            LaunchTarget::BundledTool { tool: "mkdir" },
            &[allowed_s],
        )
        .expect("build bundled command (inside)");
        assert!(run_confined(ok), "mkdir into the write prefix must succeed");
        assert!(allowed.is_dir(), "in-prefix mkdir should have landed");

        // Denial: mkdir outside the granted prefix fails, dir never appears.
        let denied = std::env::temp_dir().join(format!("ral_denied_bun_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&denied);
        let denied_s = denied.to_string_lossy().into_owned();
        let bad = macos_sandboxed_command(
            &proj,
            LaunchTarget::BundledTool { tool: "mkdir" },
            std::slice::from_ref(&denied_s),
        )
        .expect("build bundled command (outside)");
        assert!(
            !run_confined(bad),
            "mkdir outside the write prefix must fail"
        );
        assert!(
            !denied.exists(),
            "out-of-prefix mkdir must not have landed at {denied_s}"
        );

        let _ = std::fs::remove_dir_all(&work);
        let _ = std::fs::remove_dir_all(&denied);
    }

    /// Linux expands a host launch directly into the bwrap argv with the
    /// in-sandbox cwd requested via `--chdir`, ending in `-- /bin/sh …`.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_host_command_is_bwrap_with_chdir_and_target_tail() {
        let shell = Shell::default();
        let cmd = linux_sandboxed_command(
            &restrictive(),
            LaunchTarget::Host { program: "/bin/sh" },
            &["-c".into(), "echo x > /etc/ral_denied".into()],
            &shell,
        )
        .expect("build Linux host command");
        assert_eq!(cmd.get_program().to_string_lossy(), "bwrap");
        let args = argv(&cmd);
        let chdir = args
            .iter()
            .position(|a| a == "--chdir")
            .expect("--chdir present");
        assert_eq!(args[chdir + 1], shell.cwd().to_string_lossy());
        let sep = args
            .iter()
            .position(|a| a == "--")
            .expect("bwrap -- separator");
        assert!(chdir < sep, "--chdir must precede the -- separator");
        assert_eq!(args[sep + 1], "/bin/sh");
        assert_eq!(&args[sep + 2..], ["-c", "echo x > /etc/ral_denied"]);
    }
}
