//! Binary pinning for the per-command OS sandbox.
//!
//! `sandbox::boot` pins every executable the sandbox will later exec on
//! the session's behalf — ral itself everywhere, and on Linux the bwrap
//! envelope — so a launch runs the file we booted with and not whatever a
//! mid-session `cargo install`, a PATH override, or a confined child's write
//! left at the name.  Linux pins by fd and execs `/proc/self/fd/<N>`, the
//! confined trampoline included: bwrap is lent ral's pin and execs it by its
//! slot, its fresh `/proc` resolving that as any other.  macOS cannot —
//! `execve` is refused on a devfs entry, which carries no X bit — so it execs
//! the trampoline by name and re-stats `(dev, ino)` before each such spawn,
//! catching the inode flip an atomic-rename swap leaves behind; Windows,
//! confining at the parent's spawn, has no self re-exec to guard.
//!
//! `argv[0]` is always the on-disk path, whichever mechanism carried it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[cfg(target_os = "macos")]
use crate::types::Error;

// ── Pinned ───────────────────────────────────────────────────────────────

/// An executable pinned at boot, before any shell exists: the one shape a
/// `Command` the sandbox execs is ever built from, so nothing a session does
/// to its environment can choose the file.
pub(super) struct Pinned {
    #[cfg_attr(windows, allow(dead_code))]
    pin: Pin,
    /// `/proc/self/fd/<N>` on Linux, the on-disk path everywhere else.
    exec_path: PathBuf,
    arg0: PathBuf,
}

impl Pinned {
    /// Pin the executable at `arg0` for the rest of the process's life;
    /// `None` on any failure, which every caller reads as "unpinned".
    pub(super) fn open(arg0: PathBuf) -> Option<Self> {
        let (pin, exec_path) = build_pin(&arg0)?;
        Some(Self {
            pin,
            exec_path,
            arg0,
        })
    }

    /// The on-disk path the pin was taken from: `argv[0]` of every exec.
    #[cfg_attr(windows, allow(dead_code))]
    pub(super) fn arg0(&self) -> &Path {
        &self.arg0
    }

    /// What `execve` is handed to run the pinned file.
    pub(super) fn exec_path(&self) -> &Path {
        &self.exec_path
    }

    /// Exec the pinned file under its on-disk name, which `exec_path` on
    /// Linux is not.  The program is an absolute path, so the child's `PATH`
    /// — the shell's override included — is never consulted.
    #[cfg(unix)]
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:pinned-exec] Builds the Command for a boot-pinned sandbox binary (the ral re-exec for helpers and bundled tools, the bwrap envelope). Infrastructure spawn, not a model exec image — the model's exec surfaces at command::run, not here."
    )]
    pub(super) fn command(&self) -> std::process::Command {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(self.exec_path());
        cmd.arg0(self.arg0());
        cmd
    }

    /// Where the pinned inode lives *now*, after any rename since boot, so a
    /// mount meant for the file we exec lands on that file and not on
    /// whatever has since taken its name; `None` once it has no name.
    #[cfg(target_os = "linux")]
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:pin-locate] Reads the `/proc/self/fd/<N>` magic link of a boot-pinned sandbox binary to find the inode's current path for the envelope's own read-only bind. Sandbox exe-pinning infrastructure, not the model's data I/O — raises no card."
    )]
    pub(super) fn names(&self) -> std::io::Result<Option<PathBuf>> {
        let name = std::fs::read_link(self.exec_path())?;
        // Counted after the read, so an unlink in between reads as none,
        // never as ` (deleted)`.
        Ok((rustix::fs::fstat(self.fd())?.st_nlink > 0).then_some(name))
    }

    /// The pinned inode's open descriptor.
    #[cfg(target_os = "linux")]
    pub(super) fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        let Pin::Fd { fd, .. } = &self.pin;
        fd.as_fd()
    }

    /// The descriptor `exec_path` names.
    #[cfg(target_os = "linux")]
    pub(super) fn raw_fd(&self) -> std::ffi::c_int {
        use std::os::fd::AsRawFd;
        self.fd().as_raw_fd()
    }

    /// This pin again, at the lowest free descriptor from `from`: how a test
    /// puts one on a handoff slot.
    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn lifted(&self, from: std::ffi::c_int) -> Self {
        use std::os::fd::AsRawFd;
        let Pin::Fd { dev, ino, .. } = self.pin;
        let fd = rustix::io::fcntl_dupfd_cloexec(self.fd(), from).expect("a free descriptor");
        Self {
            exec_path: crate::path::proc_fd_path(fd.as_raw_fd()),
            pin: Pin::Fd { fd, dev, ino },
            arg0: self.arg0.clone(),
        }
    }

    /// Whether `meta` is the pinned inode, under whatever name — a hard link
    /// names it too.  Never on Windows, where nothing is pinned.
    #[cfg_attr(windows, allow(unused_variables))]
    pub(super) fn is_inode(&self, meta: &std::fs::Metadata) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let (dev, ino) = match &self.pin {
                #[cfg(target_os = "linux")]
                Pin::Fd { dev, ino, .. } => (*dev, *ino),
                #[cfg(not(target_os = "linux"))]
                Pin::Stat { dev, ino } => (*dev, *ino),
            };
            meta.dev() == dev && meta.ino() == ino
        }
        #[cfg(windows)]
        {
            let _ = self;
            false
        }
    }

    /// Refuse to spawn a foreign build: macOS execs the trampoline by name,
    /// so `sandbox::launch` calls this before each, to catch an executable
    /// swapped on disk since it was pinned.  Linux execs the pin itself.
    #[cfg(target_os = "macos")]
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:verify-stat] sandbox respawn guard: re-stats the pinned executable and compares (dev, ino) to catch a mid-session binary swap before re-exec; a self-path stat at respawn setup, not turn-time model data I/O, raises no surface card."
    )]
    pub(super) fn verify(&self) -> Result<(), Error> {
        let arg0 = self.arg0();
        let meta = std::fs::metadata(arg0).map_err(|e| {
            Error::new(
                format!(
                    "sandbox eval: verify self: cannot stat {}: {e}",
                    arg0.display()
                ),
                1,
            )
        })?;
        if self.is_inode(&meta) {
            Ok(())
        } else {
            Err(Error::new(
                format!(
                    "ral binary at {} changed since startup; \
                     restart to pick up the new build",
                    arg0.display()
                ),
                1,
            ))
        }
    }
}

/// How `exec_path` stays bound to the binary we pinned.
enum Pin {
    /// Never dropped, so `/proc/self/fd/<N>` keeps naming the boot inode;
    /// `(dev, ino)` is that inode's identity for [`Pinned::is_inode`].
    /// Close-on-exec, so no child inherits it: `execve` opens the target
    /// before closing the fd, which suffices for an ELF, never a `#!` script.
    #[cfg(target_os = "linux")]
    Fd {
        fd: std::os::fd::OwnedFd,
        dev: u64,
        ino: u64,
    },
    /// Compared against a fresh stat before each spawn.
    #[cfg(all(unix, not(target_os = "linux")))]
    Stat { dev: u64, ino: u64 },
    /// Nothing to guard where confinement happens at the parent's spawn; the
    /// variant only gives `build_pin` one shape on every platform.
    #[cfg(windows)]
    Unguarded,
}

pub(super) static OWN: OnceLock<Pinned> = OnceLock::new();

/// ral's own pin, which a sandboxed launch cannot go without: unpinned, ral
/// cannot vouch that the copy it re-execs is itself.
#[cfg(unix)]
pub(super) fn own() -> Result<&'static Pinned, String> {
    OWN.get().ok_or_else(|| {
        "sandbox: ral could not pin its own program at startup (is its executable \
         missing or unreadable?), so it cannot vouch that the sandboxed copy is \
         itself; refusing to launch it"
            .to_string()
    })
}

/// Pin our own executable for the rest of the process's life.
///
/// Idempotent, and silent on failure: unpinned, the unconfined helpers still
/// run, since `super::self_command` falls back to the live `current_exe`, but
/// a sandboxed launch is refused ([`own`]).
pub(super) fn pin_self() {
    if OWN.get().is_some() {
        return;
    }
    let Ok(arg0) = std::env::current_exe() else {
        return;
    };
    if let Some(pinned) = Pinned::open(arg0) {
        let _ = OWN.set(pinned);
    }
}

/// Open `arg0` and produce the `(pin, exec_path)` pair; `None` on any
/// failure, which the caller reads as "leave this binary unpinned".
#[cfg(target_os = "linux")]
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:pin-open] Opens a boot-time sandbox binary (ral itself, the bwrap envelope) to pin it by fd, immune to on-disk swaps. Sandbox exe-pinning infrastructure, not the model's data I/O — raises no card."
)]
fn build_pin(arg0: &std::path::Path) -> Option<(Pin, PathBuf)> {
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::fs::MetadataExt;

    let file = std::fs::File::open(arg0).ok()?;
    let meta = file.metadata().ok()?;
    let fd: OwnedFd = file.into();
    let exec_path = crate::path::proc_fd_path(fd.as_raw_fd());
    let pin = Pin::Fd {
        fd,
        dev: meta.dev(),
        ino: meta.ino(),
    };
    Some((pin, exec_path))
}

#[cfg(all(unix, not(target_os = "linux")))]
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:pin-stat] sandbox respawn exe-pinning: stats the running executable to capture (dev, ino) so a mid-session binary swap is detectable; a self-path stat at respawn setup, not turn-time model data I/O, raises no surface card."
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
/// `pin_self` shares with the Unix arms, which can genuinely fail.
#[cfg(windows)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the Option is the cross-platform contract of build_pin, not a claim that this arm can fail — see the doc above"
)]
fn build_pin(arg0: &std::path::Path) -> Option<(Pin, PathBuf)> {
    Some((Pin::Unguarded, arg0.to_path_buf()))
}
