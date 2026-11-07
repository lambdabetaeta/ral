//! Binary pinning for the per-command OS sandbox.
//!
//! `authority::sandbox::pin` pins every executable the sandbox will later exec on
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
//! `Pinned::command` hands every exec it builds the on-disk path as
//! `argv[0]`; bwrap execs the trampoline as `/proc/self/fd/<slot>`.

use crate::Role;
#[cfg(unix)]
use crate::path::ObjectId;
use crate::path::{LeafMode, LexicalPath, Located, Trace, walk};
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
    arg0: PathBuf,
    /// What `arg0` reached when it was pinned: the one hold on the binary,
    /// and the identity everything else reads.
    trace: Trace,
    /// Never dropped, so `/proc/self/fd/<N>` keeps naming the boot inode.
    /// Close-on-exec, so no child inherits it: `execve` opens the target
    /// before closing the fd, which suffices for an ELF, never a `#!` script.
    #[cfg(target_os = "linux")]
    fd: std::os::fd::OwnedFd,
}

impl Pinned {
    /// Pin the executable at `arg0` for the rest of the process's life;
    /// `None` on any failure, which every caller reads as "unpinned".
    pub(super) fn open(arg0: PathBuf) -> Option<Self> {
        let trace = reach(&arg0).ok()?.trace().clone();
        // An absent name pins nothing.
        trace.leaf_id()?;
        #[cfg(target_os = "linux")]
        let fd = {
            let file = site!(Silent::PinOpen, std::fs::File::open(&arg0)).ok()?;
            // The file opened must be the object `trace` holds.
            (ObjectId::of(&file).ok() == trace.leaf_id())
                .then(|| std::os::fd::OwnedFd::from(file))?
        };
        Some(Self {
            arg0,
            trace,
            #[cfg(target_os = "linux")]
            fd,
        })
    }

    /// What the pinned name reached when it was pinned.
    pub(crate) fn trace(&self) -> &Trace {
        &self.trace
    }

    /// The on-disk path the pin was taken from: `argv[0]` of every exec
    /// `Self::command` builds.
    pub(super) fn arg0(&self) -> &Path {
        &self.arg0
    }

    /// Exec the pinned file under its on-disk name, run through
    /// `/proc/self/fd/<N>` on Linux.  The program is an absolute path, so the
    /// child's `PATH` — the shell's override included — is never consulted.
    #[cfg(unix)]
    pub(super) fn command(&self) -> std::process::Command {
        use std::os::unix::process::CommandExt;
        #[cfg(target_os = "linux")]
        let program = self.proc_path();
        #[cfg(not(target_os = "linux"))]
        let program = self.arg0.clone();
        let mut cmd = site!(Silent::PinnedExec, std::process::Command::new(program));
        cmd.arg0(&self.arg0);
        cmd
    }

    /// The pinned descriptor's slot name.
    #[cfg(target_os = "linux")]
    fn proc_path(&self) -> PathBuf {
        use std::os::fd::AsRawFd;
        crate::path::proc_fd_path(self.fd.as_raw_fd())
    }

    /// Where the pinned inode lives *now*, after any rename since boot, so a
    /// mount meant for the file we exec lands on that file and not on
    /// whatever has since taken its name; `None` once it has no name.
    #[cfg(target_os = "linux")]
    pub(super) fn names(&self) -> std::io::Result<Option<PathBuf>> {
        let name = site!(Silent::PinLocate, std::fs::read_link(self.proc_path()))?;
        // Counted after the read, so an unlink in between reads as none,
        // never as ` (deleted)`.
        Ok((rustix::fs::fstat(self.fd())?.st_nlink > 0).then_some(name))
    }

    /// The pinned inode's open descriptor.
    #[cfg(target_os = "linux")]
    pub(super) fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.fd.as_fd()
    }

    /// The held object's identity, independent of its current name.
    #[cfg(unix)]
    pub(super) fn object_id(&self) -> ObjectId {
        self.trace.last_id()
    }

    /// Refuse to spawn a foreign build: macOS execs the trampoline by name,
    /// so `authority::sandbox::launch` calls this before each, to catch an executable
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
        if reached.trace().leaf_id() == Some(self.object_id()) {
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
/// a sandboxed launch is refused (`own`).
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

#[cfg(all(test, target_os = "macos"))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
