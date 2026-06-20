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
//! - **Windows**: snapshot `BY_HANDLE_FILE_INFORMATION` triple
//!   `(volume_serial, file_index_high, file_index_low)` — the NTFS
//!   analogue of `(dev, ino)`.  Verified the same way before each
//!   confined spawn.
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
    /// Exec target passed to `Command::new`.  Read on Unix only.
    #[cfg_attr(windows, allow(dead_code))]
    pub exec_path: PathBuf,
    /// `argv[0]` for the spawned child.  Read on Unix only.
    #[cfg_attr(windows, allow(dead_code))]
    pub arg0: PathBuf,
}

/// How we keep `exec_path` bound to the binary we registered.
enum Pin {
    /// Fd held purely for its `Drop`; `/proc/self/fd/<N>` resolves to the
    /// boot inode regardless of what is at the on-disk path now.
    #[cfg(target_os = "linux")]
    Fd(#[allow(dead_code)] std::os::fd::OwnedFd),
    /// macOS / other-Unix fallback: snapshot checked before each spawn.
    #[cfg(all(unix, not(target_os = "linux")))]
    Stat { dev: u64, ino: u64 },
    /// Windows NTFS analogue of `(dev, ino)`: volume serial + the two
    /// halves of the file index.  Snapshot checked before each spawn.
    #[cfg(windows)]
    NtfsStat {
        volume_serial: u32,
        index_high: u32,
        index_low: u32,
    },
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
    let stat = ntfs_stat(arg0)?;
    Some((
        Pin::NtfsStat {
            volume_serial: stat.0,
            index_high: stat.1,
            index_low: stat.2,
        },
        arg0.to_path_buf(),
    ))
}

#[cfg(windows)]
fn ntfs_stat(path: &std::path::Path) -> Option<(u32, u32, u32)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, OPEN_EXISTING,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0, // no access bits required for metadata
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info as *mut _) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return None;
    }
    Some((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
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
                "exarch binary at {} changed since startup; \
                 restart exarch to pick up the new build",
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
/// On Windows per-command process sandboxing is unimplemented: there is
/// no AppContainer / restricted-token runner.  When a policy is supplied
/// it cannot be enforced, so this entrypoint fails closed rather than
/// running unconfined.  With no policy there is nothing to enforce and it
/// returns `Ok(None)` to continue normally.
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
    if policy.is_some() {
        return Err(
            "per-command Windows sandbox not yet implemented: a requested \
             sandbox policy cannot be enforced, refusing to continue"
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
