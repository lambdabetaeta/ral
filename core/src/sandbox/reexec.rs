//! Binary pinning and re-exec for the per-command OS sandbox.
//!
//! `sandbox::early_init` pins our own executable, so a per-command sandbox
//! launch re-execs the build we booted from and not whatever a mid-session
//! `cargo install` left at the path.  Linux pins by fd and re-execs
//! `/proc/self/fd/<N>`; macOS cannot — `execve` is refused on a devfs entry,
//! which carries no X bit — so it re-stats `(dev, ino)` before each spawn,
//! catching the inode flip an atomic-rename swap leaves behind; Windows,
//! confining at the parent's spawn, has no self re-exec to guard.
//!
//! `argv[0]` is always the on-disk path, whichever mechanism carried it.

use std::path::PathBuf;
use std::sync::OnceLock;

#[cfg(target_os = "macos")]
use crate::types::Error;

// ── SandboxSelf ──────────────────────────────────────────────────────────

/// Our own executable, pinned at `early_init` time.
pub(super) struct SandboxSelf {
    #[allow(dead_code)]
    pin: Pin,
    /// `/proc/self/fd/<N>` on Linux, the on-disk path everywhere else.
    exec_path: PathBuf,
    #[cfg_attr(windows, allow(dead_code))]
    arg0: PathBuf,
}

#[cfg(unix)]
impl SandboxSelf {
    /// Re-exec the pinned binary under its on-disk name, which `exec_path`
    /// on Linux is not.
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

/// The pinned `argv[0]`, or the live `current_exe()`.  The bundled-tool
/// re-exec hands this to bwrap, which mounts a fresh `/proc` in which a
/// `/proc/self/fd` target would neither bind nor resolve.
#[cfg(target_os = "linux")]
pub(super) fn self_arg0() -> std::io::Result<PathBuf> {
    match SANDBOX_SELF.get() {
        Some(s) => Ok(s.arg0.clone()),
        None => std::env::current_exe(),
    }
}

/// The pinned exec path for the Seatbelt `(literal …)` self-admit clause
/// that lets a bundled-tool re-exec run under a restricted profile.
#[cfg(target_os = "macos")]
pub(super) fn self_exec_path_string() -> Option<String> {
    SANDBOX_SELF
        .get()
        .map(|s| s.exec_path.to_string_lossy().into_owned())
}

/// The exec path `windows::session::confine` stamps readable for the
/// `AppContainer`, so the bundled-tool re-exec can load its own image.
/// `None` leaves it ungranted, readable only if the fs projection covers it.
#[cfg(windows)]
pub(super) fn self_exec_path() -> Option<PathBuf> {
    if let Some(s) = SANDBOX_SELF.get() {
        return Some(s.exec_path.clone());
    }
    std::env::current_exe().ok()
}

/// How `exec_path` stays bound to the binary we registered.
enum Pin {
    /// Never dropped, so `/proc/self/fd/<N>` keeps naming the boot inode.
    #[cfg(target_os = "linux")]
    Fd(#[allow(dead_code)] std::os::fd::OwnedFd),
    /// Compared against a fresh stat before each spawn.
    #[cfg(all(unix, not(target_os = "linux")))]
    Stat { dev: u64, ino: u64 },
    /// Nothing to guard where confinement happens at the parent's spawn; the
    /// variant only gives `build_pin` one shape on every platform.
    #[cfg(windows)]
    Unguarded,
}

pub(super) static SANDBOX_SELF: OnceLock<SandboxSelf> = OnceLock::new();

/// Pin our own executable for the rest of the process's life.
///
/// Idempotent, and silent on failure: unpinned we still sandbox, since
/// `super::self_command` falls back to the live `current_exe`, but we lose
/// swap protection and, on macOS, the Seatbelt self-admit clause.
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

/// Open `arg0` and produce the `(pin, exec_path)` pair; `None` on any
/// failure, which the caller reads as "leave this binary unpinned".
#[cfg(target_os = "linux")]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:pin-open] Opens the boot-time ral binary to pin it (by fd) for the Linux sandbox respawn, immune to on-disk swaps. Sandbox exe-pinning infrastructure, not the model's data I/O — raises no card."
)]
fn build_pin(arg0: &std::path::Path) -> Option<(Pin, PathBuf)> {
    use std::os::fd::{AsRawFd, OwnedFd};

    let exe: OwnedFd = std::fs::File::open(arg0).ok()?.into();
    // Rust opens with FD_CLOEXEC set; clear it, or the fd dies in the
    // execve and `/proc/self/fd/<N>` has nothing to resolve to.
    let raw = exe.as_raw_fd();
    let _ = rustix::io::fcntl_setfd(&exe, rustix::io::FdFlags::empty());
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

/// Windows pinning cannot fail because it does nothing: no fd to open, no
/// inode to snapshot, only the path to carry.  The `Option` is the shape
/// `register_sandbox_self` shares with the Unix arms, which can genuinely
/// fail.
#[cfg(windows)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the Option is the cross-platform contract of build_pin, not a claim that this arm can fail — see the doc above"
)]
fn build_pin(arg0: &std::path::Path) -> Option<(Pin, PathBuf)> {
    Some((Pin::Unguarded, arg0.to_path_buf()))
}

/// Refuse to spawn a foreign build: `sandbox::launch` calls this before
/// every per-command `--sandbox-projection` re-exec, to catch an executable
/// swapped on disk since registration.
///
/// macOS-only, the lone platform with a parent-side self re-exec — Linux
/// goes through the fd-pinned path, out of a swap's reach, and Windows never
/// re-execs itself — so `Pin::Stat` is the only variant compiled here.
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
/// `Some(code)` means exit with it now: on Linux the respawn under bwrap has
/// already run the real work in a child.
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
    // Confinement is the `AppContainer` token the parent stages via
    // `Launch::security_capabilities` before `CreateProcessW`, so a child has
    // nothing to enter and `sandbox::launch` never emits the flag.  Seeing
    // one means a caller regressed to the Unix shape, or forged it.
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
