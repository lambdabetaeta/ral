//! Binary pinning for the per-command OS sandbox.
//!
//! `sandbox::pin` pins every executable the sandbox will later exec on
//! the session's behalf — ral itself everywhere, and on Linux the bwrap
//! envelope — so a launch runs the file we booted with and not whatever a
//! mid-session `cargo install`, a PATH override, or a confined child's write
//! left at the name.  Linux pins by fd and execs `/proc/self/fd/<N>`, the
//! confined trampoline included: bwrap is lent ral's pin and execs it by its
//! slot, its fresh `/proc` resolving that as any other.  macOS cannot —
//! `execve` is refused on a devfs entry, which carries no X bit — so it execs
//! the trampoline by name and walks that name again for its identity before
//! each such spawn, catching the inode flip an atomic-rename swap leaves
//! behind; Windows, confining at the parent's spawn, has no self re-exec to
//! guard.
//!
//! [`Pinned::command`] hands every exec it builds the on-disk path as
//! `argv[0]`; bwrap execs the trampoline as `/proc/self/fd/<slot>`.

use crate::Role;
use crate::path::{LeafMode, LexicalPath, Located, ObjectId, Trace, walk};
#[cfg(unix)]
use crate::site::{Silent, site};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// ── Pinned ───────────────────────────────────────────────────────────────

/// An executable pinned at boot, before any shell exists: the one shape a
/// `Command` the sandbox execs is ever built from, so nothing a session does
/// to its environment can choose the file.
pub(super) struct Pinned {
    #[cfg_attr(windows, allow(dead_code))]
    pin: Pin,
    /// `/proc/self/fd/<N>` on Linux, the on-disk path everywhere else.
    #[cfg_attr(windows, allow(dead_code))]
    exec_path: PathBuf,
    arg0: PathBuf,
    /// What `arg0` reached when it was pinned: the one hold on the binary.
    trace: Trace,
}

impl Pinned {
    /// Pin the executable at `arg0` for the rest of the process's life;
    /// `None` on any failure, which every caller reads as "unpinned".
    pub(super) fn open(arg0: PathBuf) -> Option<Self> {
        let trace = reach(&arg0).ok()?.trace().clone();
        let (pin, exec_path) = build_pin(&arg0, &trace)?;
        Some(Self {
            pin,
            exec_path,
            arg0,
            trace,
        })
    }

    /// What the pinned name reached when it was pinned.
    pub(crate) fn trace(&self) -> &Trace {
        &self.trace
    }

    /// The on-disk path the pin was taken from: `argv[0]` of every exec
    /// [`Self::command`] builds.
    pub(super) fn arg0(&self) -> &Path {
        &self.arg0
    }

    /// What `execve` is handed to run the pinned file.
    #[cfg(unix)]
    pub(super) fn exec_path(&self) -> &Path {
        &self.exec_path
    }

    /// Exec the pinned file under its on-disk name, which `exec_path` on
    /// Linux is not.  The program is an absolute path, so the child's `PATH`
    /// — the shell's override included — is never consulted.
    #[cfg(unix)]
    pub(super) fn command(&self) -> std::process::Command {
        use std::os::unix::process::CommandExt;
        let mut cmd = site!(
            Silent::PinnedExec,
            std::process::Command::new(self.exec_path())
        );
        cmd.arg0(self.arg0());
        cmd
    }

    /// Where the pinned inode lives *now*, after any rename since boot, so a
    /// mount meant for the file we exec lands on that file and not on
    /// whatever has since taken its name; `None` once it has no name.
    #[cfg(target_os = "linux")]
    pub(super) fn names(&self) -> std::io::Result<Option<PathBuf>> {
        let name = site!(Silent::PinLocate, std::fs::read_link(self.exec_path()))?;
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

    /// This pin again, at the lowest free descriptor from `from`: how a test
    /// puts one on a handoff slot.
    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn lifted(&self, from: std::ffi::c_int) -> Self {
        use std::os::fd::AsRawFd;
        let Pin::Fd { id, .. } = self.pin;
        let fd = rustix::io::fcntl_dupfd_cloexec(self.fd(), from).expect("a free descriptor");
        Self {
            exec_path: crate::path::proc_fd_path(fd.as_raw_fd()),
            pin: Pin::Fd { fd, id },
            arg0: self.arg0.clone(),
            trace: self.trace.clone(),
        }
    }

    /// Whether `id` is the pinned object, under whatever name — a hard link
    /// names it too.  Never on Windows, where nothing is pinned.
    #[cfg_attr(windows, allow(unused_variables))]
    pub(super) fn is_object_id(&self, id: ObjectId) -> bool {
        #[cfg(unix)]
        {
            match &self.pin {
                #[cfg(target_os = "linux")]
                Pin::Fd { id: pinned, .. } => *pinned == id,
                #[cfg(not(target_os = "linux"))]
                Pin::Stat { id: pinned } => *pinned == id,
            }
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
    pub(super) fn verify(&self) -> Result<(), String> {
        let arg0 = self.arg0();
        let reached = reach(arg0).map_err(|e| {
            format!(
                "sandbox eval: verify self: cannot reach {}: {e}",
                crate::path::shown(arg0)
            )
        })?;
        if (reached.trace().leaf_id()).is_some_and(|id| self.is_object_id(id)) {
            Ok(())
        } else {
            Err(format!(
                "ral binary at {} changed since startup; \
                     restart to pick up the new build",
                crate::path::shown(arg0)
            ))
        }
    }
}

/// Where the absolute name `arg0` leads now.
fn reach(arg0: &Path) -> io::Result<Located> {
    let at = LexicalPath::absolute(arg0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a pinned name is absolute"))?;
    walk(&at, LeafMode::Follow)
}

/// How `exec_path` stays bound to the binary we pinned.
enum Pin {
    /// Never dropped, so `/proc/self/fd/<N>` keeps naming the boot inode;
    /// `id` is that inode's identity for [`Pinned::is_object_id`].
    /// Close-on-exec, so no child inherits it: `execve` opens the target
    /// before closing the fd, which suffices for an ELF, never a `#!` script.
    #[cfg(target_os = "linux")]
    Fd {
        fd: std::os::fd::OwnedFd,
        id: ObjectId,
    },
    /// Compared against a fresh stat before each spawn.
    #[cfg(all(unix, not(target_os = "linux")))]
    Stat { id: ObjectId },
    /// Nothing to guard where confinement happens at the parent's spawn; the
    /// variant only gives `build_pin` one shape on every platform.
    #[cfg(windows)]
    Unguarded,
}

pub(super) static OWN: OnceLock<Pinned> = OnceLock::new();

/// What ral's own binary reached when it was pinned at boot, the one hold on
/// it; `None` while unpinned.
pub(crate) fn own_binary() -> Option<&'static Trace> {
    OWN.get().map(Pinned::trace)
}

/// The path ral's own binary was pinned from at boot, for a guard that names
/// it; `None` while unpinned.
pub fn pinned_arg0() -> Option<&'static Path> {
    OWN.get().map(Pinned::arg0)
}

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

/// This binary re-exec'd in `role`, through the boot pin when there is one, so
/// a helper stays bound to the boot-time build even if the on-disk path is
/// swapped.
#[cfg(unix)]
pub(crate) fn launch(role: Role) -> std::io::Result<crate::process::Launch> {
    let mut cmd = if let Some(own) = OWN.get() {
        own.command()
    } else {
        site!(
            Silent::SelfReexec,
            std::process::Command::new(std::env::current_exe()?)
        )
    };
    cmd.arg(role.flag());
    Ok(crate::process::Launch::from_command(cmd))
}

/// [`launch`] on Windows, which has no sandbox-pinned self-path: it takes the
/// live `current_exe` and would follow an on-disk swap between launch and exec.
#[cfg(windows)]
pub(crate) fn launch(role: Role) -> std::io::Result<crate::process::Launch> {
    let mut cmd = crate::process::Launch::new(std::env::current_exe()?);
    cmd.arg(role.flag());
    Ok(cmd)
}

/// Pin our own executable for the rest of the process's life.
///
/// Idempotent, and silent on failure: unpinned, the unconfined helpers still
/// run, since [`launch`] falls back to the live `current_exe`, but
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
/// failure, which the caller reads as "leave this binary unpinned".  The
/// file opened must be the object `trace` holds.
#[cfg(target_os = "linux")]
fn build_pin(arg0: &Path, trace: &Trace) -> Option<(Pin, PathBuf)> {
    use std::os::fd::{AsRawFd, OwnedFd};

    let file = site!(Silent::PinOpen, std::fs::File::open(arg0)).ok()?;
    let id = ObjectId::of(&file).ok()?;
    if trace.leaf_id() != Some(id) {
        return None;
    }
    let fd: OwnedFd = file.into();
    let exec_path = crate::path::proc_fd_path(fd.as_raw_fd());
    Some((Pin::Fd { fd, id }, exec_path))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn build_pin(arg0: &Path, trace: &Trace) -> Option<(Pin, PathBuf)> {
    Some((
        Pin::Stat {
            id: trace.leaf_id()?,
        },
        arg0.to_path_buf(),
    ))
}

/// Windows pinning cannot fail because it does nothing: no fd to open, no
/// inode to snapshot, only the path to carry.  The `Option` is the shape
/// `pin_self` shares with the Unix arms, which can genuinely fail.
#[cfg(windows)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the Option is the cross-platform contract of build_pin, not a claim that this arm can fail: see the doc above"
)]
fn build_pin(arg0: &Path, _: &Trace) -> Option<(Pin, PathBuf)> {
    Some((Pin::Unguarded, arg0.to_path_buf()))
}
