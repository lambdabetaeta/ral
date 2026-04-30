//! Binary pinning and re-exec for sandboxed grant blocks.
//!
//! At [`crate::sandbox::early_init`] we capture an immutable handle on
//! our own executable so restrictive grant blocks can re-exec the same
//! binary, immune to on-disk swaps (`cargo install` mid-session, package
//! upgrades) that would produce IPC wire-format mismatches.
//!
//! ## Platform strategies
//!
//! - **Linux**: open the file as `OwnedFd` (no `O_CLOEXEC`) and re-exec
//!   via `/proc/self/fd/<N>`.  The kernel resolves through our fd table to
//!   the original inode; protection is total.
//! - **macOS / other Unix**: the kernel refuses `execve("/dev/fd/<N>", …)`
//!   (devfs entries lack the X bit), so we snapshot `(dev, ino)` at boot
//!   and verify immediately before each spawn.  An atomic-rename swap
//!   (the cargo-install pattern) flips the inode, which we catch.  TOCTOU
//!   window is microseconds and bounded by an explicit check.  `mtime` is
//!   deliberately not part of the pin: macOS metadata churn (codesign
//!   re-signing, Spotlight, antivirus, xattr quarantine flips) updates it
//!   without changing content, and `utime(2)` makes mtime trivially
//!   forgeable, so it would only generate false positives.
//!
//! `argv[0]` is the resolved on-disk path so the child sees a recognisable
//! name regardless of exec mechanism.  The previous approach (env var
//! `RAL_SANDBOX_SELF_BIN`) was poisoned by inherited shells; in-process
//! state is the right shape.

use std::path::PathBuf;
use std::sync::OnceLock;

#[cfg(all(unix, not(target_os = "linux")))]
use crate::types::Error;
use crate::types::EvalSignal;

// ── SandboxSelf ──────────────────────────────────────────────────────────

/// Our own executable, pinned at `early_init` time.
#[cfg(unix)]
pub(super) struct SandboxSelf {
    /// Platform anchor binding `exec_path` to the boot inode.
    pin: Pin,
    /// Exec target passed to `Command::new`.
    pub exec_path: PathBuf,
    /// `argv[0]` for the spawned child.
    pub arg0: PathBuf,
}

/// How we keep `exec_path` bound to the binary we registered.
#[cfg(unix)]
enum Pin {
    /// Fd held purely for its `Drop`; `/proc/self/fd/<N>` resolves to the
    /// boot inode regardless of what is at the on-disk path now.
    #[cfg(target_os = "linux")]
    Fd(#[allow(dead_code)] std::os::fd::OwnedFd),
    /// macOS / other-Unix fallback: snapshot checked before each spawn.
    #[cfg(all(unix, not(target_os = "linux")))]
    Stat { dev: u64, ino: u64 },
}

#[cfg(unix)]
pub(super) static SANDBOX_SELF: OnceLock<SandboxSelf> = OnceLock::new();

/// Pin our own executable for the rest of the process's life.
///
/// Idempotent; failures are silent — sandbox-capable becomes false and
/// restrictive grants fall through to in-process evaluation.
#[cfg(unix)]
pub(super) fn register_sandbox_self() {
    if SANDBOX_SELF.get().is_some() {
        return;
    }
    let Ok(arg0) = std::env::current_exe() else { return };
    let Some((pin, exec_path)) = build_pin(&arg0) else { return };
    let _ = SANDBOX_SELF.set(SandboxSelf { pin, exec_path, arg0 });
}

#[cfg(not(unix))]
pub(super) fn register_sandbox_self() {}

/// Open `arg0` and produce the `(pin, exec_path)` pair for binary-swap
/// protection.  Returns `None` on any failure; the caller treats that as
/// "binary not sandbox-capable."
#[cfg(target_os = "linux")]
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
    Some((Pin::Fd(exe), PathBuf::from(format!("/proc/self/fd/{raw}"))))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn build_pin(arg0: &std::path::Path) -> Option<(Pin, PathBuf)> {
    use std::os::unix::fs::MetadataExt;

    let meta = std::fs::metadata(arg0).ok()?;
    let pin = Pin::Stat {
        dev: meta.dev(),
        ino: meta.ino(),
    };
    Some((pin, arg0.to_path_buf()))
}

/// Refuse to spawn if our executable on disk has been swapped since
/// registration.  `Pin::Fd` is a no-op — the fd-derived exec path makes
/// swaps irrelevant.
#[cfg(unix)]
pub(super) fn verify_unswapped(s: &SandboxSelf) -> Result<(), EvalSignal> {
    match &s.pin {
        #[cfg(target_os = "linux")]
        Pin::Fd(_) => Ok(()),
        #[cfg(all(unix, not(target_os = "linux")))]
        Pin::Stat { dev, ino } => {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(&s.arg0).map_err(|e| {
                crate::sandbox::runner::stage_err(
                    "verify self",
                    format!("cannot stat {}: {e}", s.arg0.display()),
                )
            })?;
            if meta.dev() == *dev && meta.ino() == *ino {
                Ok(())
            } else {
                Err(EvalSignal::Error(Error::new(
                    format!(
                        "exarch binary at {} changed since startup; \
                         restart exarch to pick up the new build",
                        s.arg0.display()
                    ),
                    1,
                )))
            }
        }
    }
}

// ── Process sandbox entry ────────────────────────────────────────────────

/// Enter the OS sandbox for this process if a policy was supplied.
/// Returns `Some(code)` when the process should exit immediately (Linux
/// bwrap respawn path), `None` to continue normally.
pub(super) fn maybe_enter_process_sandbox(
    args_without_policy: &[String],
    policy: Option<&crate::types::SandboxProjection>,
) -> Result<Option<std::process::ExitCode>, String> {
    let Some(policy) = policy else {
        return Ok(None);
    };
    enter_for_platform(args_without_policy, policy)
}

#[cfg(target_os = "macos")]
fn enter_for_platform(
    _args: &[String],
    policy: &crate::types::SandboxProjection,
) -> Result<Option<std::process::ExitCode>, String> {
    super::macos::enter_current_process(policy, super::SANDBOX_ACTIVE_ENV)?;
    Ok(None)
}

#[cfg(target_os = "linux")]
fn enter_for_platform(
    args: &[String],
    policy: &crate::types::SandboxProjection,
) -> Result<Option<std::process::ExitCode>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("ral: current_exe: {e}"))?;
    super::linux::respawn_under_bwrap(&exe, args, policy, super::SANDBOX_ACTIVE_ENV).map(Some)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn enter_for_platform(
    _args: &[String],
    _policy: &crate::types::SandboxProjection,
) -> Result<Option<std::process::ExitCode>, String> {
    Err("ral: fs/net sandboxing is unavailable on this Unix platform".into())
}

/// Detect `--internal-sandbox-block` in `args` and run the IPC child
/// loop if present.  Returns `Some(code)` to signal immediate exit.
pub(super) fn maybe_handle_internal_mode(args: &[String]) -> Option<std::process::ExitCode> {
    if args.first().map(|a| a.as_str()) != Some(super::runner::INTERNAL_BLOCK_MODE) {
        return None;
    }
    Some(super::ipc::serve_from_env_fd())
}
