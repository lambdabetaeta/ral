//! Binary pinning and re-exec for the per-command OS sandbox.
//!
//! At [`crate::sandbox::early_init`] we capture an immutable handle on
//! our own executable so a per-command sandbox launch can re-exec the
//! same binary, immune to on-disk swaps (`cargo install` mid-session,
//! package upgrades) that would re-exec a foreign build.
//!
//! ## Platform strategies
//!
//! - **Linux**: open the file as `OwnedFd` (no `O_CLOEXEC`) and re-exec
//!   via `/proc/self/fd/<N>`.  The kernel resolves through our fd table to
//!   the original inode; protection is total.
//! - **macOS / other Unix**: the kernel refuses `execve("/dev/fd/<N>", …)`
//!   (devfs entries lack the X bit), so we snapshot `(dev, ino)` at boot
//!   and verify immediately before each spawn.  An atomic-rename swap
//!   (the cargo-install pattern) flips the inode, which we catch.
//! - **Windows**: no swap guard. Confinement is the AppContainer token the
//!   *parent* applies to `CreateProcessW` (`sandbox::windows::session`),
//!   not a re-exec of this binary, so there is no parent-side self re-exec
//!   for a swap check to protect (`Pin::Unguarded` below).
//!
//! `argv[0]` is the resolved on-disk path so the child sees a recognisable
//! name regardless of exec mechanism.

use std::path::PathBuf;
use std::sync::OnceLock;

#[cfg(target_os = "macos")]
use crate::types::Error;

// ── SandboxSelf ──────────────────────────────────────────────────────────

/// Our own executable, pinned at `early_init` time.
pub(super) struct SandboxSelf {
    /// Platform anchor binding `exec_path` to the boot inode.
    #[allow(dead_code)]
    pin: Pin,
    /// Exec target passed to `Command::new` on Unix, and — on Windows —
    /// the bundled-tool self-reexec image the AppContainer is granted RO
    /// access to (see [`self_exec_path`]).
    exec_path: PathBuf,
    /// `argv[0]` for the spawned child.  Read on Unix only.
    #[cfg_attr(windows, allow(dead_code))]
    arg0: PathBuf,
}

#[cfg(unix)]
impl SandboxSelf {
    /// Build a re-exec `Command` for this pinned binary: exec the pinned
    /// path with the on-disk name as `argv[0]` so the child sees a
    /// recognisable name regardless of the exec mechanism.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:self-reexec] Builds the ral-re-exec Command for sandbox helper subprocesses (pipeline helper, bundled-tool multicall). Infrastructure spawn, not a model exec image — the model's exec surfaces at command::run, not here."
    )]
    pub(super) fn reexec_command(&self) -> std::process::Command {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(&self.exec_path);
        cmd.arg0(&self.arg0);
        cmd
    }
}

/// The pinned self `argv[0]` (on-disk path), or `current_exe()` when the
/// binary was not pinned.  Used by the Linux bundled-tool re-exec, which
/// hands bwrap the on-disk path (a `/proc/self/fd` target would neither
/// bind nor resolve in bwrap's fresh `/proc`).
///
/// # Errors
/// Returns `Err` if the binary was not pinned and `current_exe()` fails.
#[cfg(target_os = "linux")]
pub(super) fn self_arg0() -> std::io::Result<PathBuf> {
    match SANDBOX_SELF.get() {
        Some(s) => Ok(s.arg0.clone()),
        None => std::env::current_exe(),
    }
}

/// The pinned self exec path as a string, for the macOS Seatbelt
/// `(literal …)` self-admit clause.  `None` when the binary was not
/// pinned (sandbox-incapable).
#[cfg(target_os = "macos")]
pub(super) fn self_exec_path_string() -> Option<String> {
    SANDBOX_SELF
        .get()
        .map(|s| s.exec_path.to_string_lossy().into_owned())
}

/// The pinned self exec path (Windows), for granting the AppContainer read
/// access to the bundled-tool self-reexec image (`ral.exe`).  Falls back to
/// the live `current_exe` when the binary was not pinned; `None` only if that
/// also fails, in which case the image is left ungranted and its readability
/// rests on the fs read projection.
#[cfg(windows)]
pub(super) fn self_exec_path() -> Option<PathBuf> {
    if let Some(s) = SANDBOX_SELF.get() {
        return Some(s.exec_path.clone());
    }
    std::env::current_exe().ok()
}

/// How we keep `exec_path` bound to the binary we registered.
enum Pin {
    /// Fd retained (never dropped) so it stays open for the process
    /// lifetime; `/proc/self/fd/<N>` then resolves to the boot inode
    /// regardless of what is at the on-disk path now.
    #[cfg(target_os = "linux")]
    Fd(#[allow(dead_code)] std::os::fd::OwnedFd),
    /// macOS / other-Unix fallback: snapshot checked before each spawn.
    #[cfg(all(unix, not(target_os = "linux")))]
    Stat { dev: u64, ino: u64 },
    /// Windows carries no swap guard: confinement applies the AppContainer
    /// token to the parent's spawn rather than re-execing ral, so there is
    /// no parent-side self re-exec for a swap check to protect (the
    /// bundled-tool multicall uses the live `current_exe` and, being
    /// short-lived, does not guard swaps — see
    /// `runtime::pipeline::helper::self_reexec`). This variant carries no
    /// payload; it exists only so `register_sandbox_self` has the same
    /// `Option<(Pin, PathBuf)>` shape on every platform.
    #[cfg(windows)]
    Unguarded,
}

pub(super) static SANDBOX_SELF: OnceLock<SandboxSelf> = OnceLock::new();

/// Pin our own executable for the rest of the process's life.
///
/// Idempotent; failures are silent — sandbox-capable becomes false and
/// restrictive grants fall through to in-process evaluation.
pub(super) fn register_sandbox_self() {
    if SANDBOX_SELF.get().is_some() {
        return;
    }
    let Ok(arg0) = std::env::current_exe() else {
        return;
    };
    let Some((pin, exec_path)) = build_pin(&arg0) else {
        return;
    };
    let _ = SANDBOX_SELF.set(SandboxSelf {
        pin,
        exec_path,
        arg0,
    });
}

/// Open `arg0` and produce the `(pin, exec_path)` pair for binary-swap
/// protection.  Returns `None` on any failure; the caller treats that as
/// "binary not sandbox-capable."
#[cfg(target_os = "linux")]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:pin-open] Opens the boot-time ral binary to pin it (by fd) for the Linux sandbox respawn, immune to on-disk swaps. Sandbox exe-pinning infrastructure, not the model's data I/O — raises no card."
)]
fn build_pin(arg0: &std::path::Path) -> Option<(Pin, PathBuf)> {
    use std::os::fd::{AsRawFd, OwnedFd};

    let exe: OwnedFd = std::fs::File::open(arg0).ok()?.into();
    // Clear FD_CLOEXEC so the fd survives execve into the sandbox child,
    // where `/proc/self/fd/<N>` needs to resolve.
    let raw = exe.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
    }
    Some((Pin::Fd(exe), crate::path::proc_fd_path(raw)))
}

#[cfg(all(unix, not(target_os = "linux")))]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:pin-stat] sandbox respawn exe-pinning: stats the running executable to capture (dev, ino) so a mid-session binary swap is detectable; a self-path stat at respawn setup, not turn-time model data I/O, raises no surface card."
)]
fn build_pin(arg0: &std::path::Path) -> Option<(Pin, PathBuf)> {
    use std::os::unix::fs::MetadataExt;

    let meta = std::fs::metadata(arg0).ok()?;
    let pin = Pin::Stat {
        dev: meta.dev(),
        ino: meta.ino(),
    };
    Some((pin, arg0.to_path_buf()))
}

#[cfg(windows)]
fn build_pin(arg0: &std::path::Path) -> Option<(Pin, PathBuf)> {
    Some((Pin::Unguarded, arg0.to_path_buf()))
}

/// Refuse to spawn if our executable on disk has been swapped since
/// registration.  Called by the per-command self re-exec before spawning
/// `--sandbox-projection` so a mid-session binary swap is caught rather
/// than silently re-execing a foreign build.
///
/// macOS-only: it is the lone platform with a parent-side per-command self
/// re-exec (Seatbelt entry).  Linux re-execs through the fd-pinned
/// `/proc/self/fd/N` so a swap is already irrelevant and bwrap is launched
/// directly, never via this guard; Windows has no parent-side self re-exec.
/// macOS pins via `(dev, ino)` ([`Pin::Stat`], the only variant compiled
/// here), so the guard re-stats and compares.
#[cfg(target_os = "macos")]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:verify-stat] sandbox respawn guard: re-stats the pinned executable and compares (dev, ino) to catch a mid-session binary swap before re-exec; a self-path stat at respawn setup, not turn-time model data I/O, raises no surface card."
)]
pub(super) fn verify_unswapped(s: &SandboxSelf) -> Result<(), Error> {
    use std::os::unix::fs::MetadataExt;
    let Pin::Stat { dev, ino } = &s.pin;
    let meta = std::fs::metadata(&s.arg0).map_err(|e| {
        Error::new(
            format!(
                "sandbox eval: verify self: cannot stat {}: {e}",
                s.arg0.display()
            ),
            1,
        )
    })?;
    if meta.dev() == *dev && meta.ino() == *ino {
        Ok(())
    } else {
        Err(Error::new(
            format!(
                "ral binary at {} changed since startup; \
                 restart to pick up the new build",
                s.arg0.display()
            ),
            1,
        ))
    }
}

// ── Process sandbox entry ────────────────────────────────────────────────

/// Enter the OS sandbox for this process if a policy was supplied.
/// Returns `Some(code)` when the process should exit immediately (Linux
/// bwrap respawn path), `None` to continue normally.
///
/// On Windows there is no child re-entry: confinement is not something the
/// child enters, it is the LowBox token the *parent* applies at spawn time
/// via `Launch::security_capabilities` (see `sandbox::windows::session`). So
/// `--sandbox-projection` is never passed to a Windows child by any
/// legitimate caller — the separate `#[cfg(windows)]` arm below treats
/// seeing one anyway as fail-closed, not a no-op.
#[cfg(unix)]
pub(super) fn maybe_enter_process_sandbox(
    args_without_policy: &[String],
    policy: Option<&crate::types::SandboxProjection>,
) -> Result<Option<u8>, String> {
    let Some(policy) = policy else {
        return Ok(None);
    };
    enter_for_platform(args_without_policy, policy)
}

#[cfg(windows)]
pub(super) fn maybe_enter_process_sandbox(
    _args_without_policy: &[String],
    policy: Option<&crate::types::SandboxProjection>,
) -> Result<Option<u8>, String> {
    // The parent already confines this child's spawn with the AppContainer
    // token before `CreateProcessW` runs, so there is no sandbox for the
    // child itself to enter — every legitimate Windows caller
    // (`sandboxed_command`'s windows arm, `launch.rs`) therefore never emits
    // `--sandbox-projection`. Seeing one here means a caller regressed to
    // the Unix shape or the flag was forged; fail closed rather than
    // silently running the child unconfined, matching the pre-W1 stub's
    // refusal on this same path.
    if policy.is_some() {
        return Err(
            "ral: --sandbox-projection is not valid on Windows (confinement is applied by the \
             parent at spawn time, not entered by the child); refusing to run unconfined"
                .into(),
        );
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
fn enter_for_platform(
    _args: &[String],
    policy: &crate::types::SandboxProjection,
) -> Result<Option<u8>, String> {
    super::macos::enter_current_process(policy)?;
    Ok(None)
}

#[cfg(target_os = "linux")]
fn enter_for_platform(
    args: &[String],
    policy: &crate::types::SandboxProjection,
) -> Result<Option<u8>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("ral: current_exe: {e}"))?;
    super::linux::respawn_under_bwrap(&exe, args, policy).map(Some)
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn enter_for_platform(
    _args: &[String],
    _policy: &crate::types::SandboxProjection,
) -> Result<Option<u8>, String> {
    Err("ral: fs/net sandboxing is unavailable on this Unix platform".into())
}
