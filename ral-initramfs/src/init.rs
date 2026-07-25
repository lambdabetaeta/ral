//! The syscall sequence [`run`] carries out.
//!
//! Mirrors `ral-daemon/src/init.rs`'s own split: [`crate::plan`] is the
//! decision, this is only the thin edge that performs it — once, with no
//! supervision loop, because nothing runs yet to supervise.

use std::convert::Infallible;
use std::fs;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::Command;

use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, mkdir, openat, stat, statat, unlinkat,
};
use rustix::io::Errno;
use rustix::mount::{mount, mount_move};
use rustix::process::{chdir, chroot};
use rustix::stdio::{dup2_stderr, dup2_stdin, dup2_stdout};
use rustix::system::finit_module;

use crate::plan::{self, Install, Mount};

/// Be the guest's `/init`.
///
/// Find the session's two disks, format the writable one, assemble the
/// overlay root, install the two binaries `rootfs.img` does not carry, and
/// hand off to `ral-daemon` as the new pid 1.
///
/// Returns only on failure — success replaces this process image with
/// `ral-daemon` inside [`switch_root`], so there is nothing to return to.
///
/// # Errors
/// Returns a sentence naming the step that failed.
pub fn run() -> Result<Infallible, String> {
    apply(&plan::DEV)?;
    load_modules()?;
    adopt_console()?;
    // Only now can the disks be found: the nodes exist because `devtmpfs` is
    // up and the block driver is loaded.  The console comes first too, so a
    // machine with no disks says so somewhere a maintainer can read it.
    let disks = await_disks()?;
    format_session(disks.session)?;
    for disk in plan::disk_mounts(disks) {
        apply(&disk)?;
    }
    for dir in [plan::UPPER, plan::WORKDIR] {
        make_dir(dir)?;
    }
    apply(&plan::OVERLAY)?;
    for install in plan::INSTALLS {
        install_binary(&install)?;
    }
    switch_root()
}

/// Load the kernel drivers neither disk, the overlay, nor a virtio-vsock
/// dial can work without.
///
/// A missing [`plan::MODULE_MANIFEST`] means the pinned kernel builds every
/// needed driver in rather than as a module — a legitimate outcome, not a
/// failure, so it is skipped rather than refused. A manifest that names a
/// module this initramfs did not carry, or that the kernel will not load at
/// all, is: nothing past this point can work without it. The image carries only
/// the modules its own hypervisor's transport needs (`build-boot.sh` ships
/// virtio's for the arm64 image, Hyper-V's for the Windows one), so every
/// module named here is required.
///
/// # The order in the manifest is a hint, not a contract
///
/// `build-boot.sh` computes the manifest as a topological order over the
/// dependency graph `modules.dep` describes, so in the ordinary case one pass
/// loads everything. It is not trusted to, because it has been wrong: it once
/// listed `9pnet` before `netfs`, whose symbols `9pnet` needs — a modules.dep
/// line lists a module's dependencies as a set, and reading that set as a load
/// order killed the guest at `Attempted to kill init!` a hundred milliseconds
/// into its boot. What that order rests on is the kernel's own metadata about
/// which module exports which symbol, none of which this repository can check.
/// So loading does not depend on it. A module whose dependencies are not yet in
/// the kernel fails with `ENOENT` — the kernel's way of saying "unknown symbol"
/// — and the very same module loads without complaint once whatever exports
/// that symbol is in. Passes are therefore repeated while they make progress:
/// convergence takes as many passes as the dependency chain is deep, and a pass
/// that loads nothing ends it. Whether a metadata file got the order right is
/// the sort of thing to be wrong about; whether a symbol resolves is not.
fn load_modules() -> Result<(), String> {
    let Ok(manifest) = fs::read_to_string(plan::MODULE_MANIFEST) else {
        return Ok(());
    };
    let mut pending: Vec<&str> = manifest.lines().filter(|line| !line.is_empty()).collect();
    let mut errors: Vec<String> = Vec::new();

    while !pending.is_empty() {
        let mut stuck = Vec::new();
        errors.clear();
        for name in &pending {
            let path = format!("{}/{name}", plan::MODULE_DIR);
            let file =
                fs::File::open(&path).map_err(|err| format!("could not open {path}: {err}"))?;
            match finit_module(&file, c"", 0) {
                // Already in the kernel — built in after all, or pulled in by a
                // driver the kernel bound itself.  The point of loading a module
                // is that it *be* loaded, so finding it there is the goal
                // reached early, not a failure.
                Ok(()) | Err(Errno::EXIST) => {}
                Err(err) => {
                    errors.push(format!("{name} ({err})"));
                    stuck.push(*name);
                }
            }
        }
        // A pass that loaded nothing will load nothing next time either: the
        // kernel state it would need is exactly the state this pass failed to
        // produce.  Anything less than that is progress, and worth another pass.
        if stuck.len() == pending.len() {
            break;
        }
        pending = stuck;
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "these kernel modules would not load, and nothing past this point works without \
             them: {}",
            errors.join(", ")
        ))
    }
}

/// Point this process — and everything it execs — at the real console.
///
/// `console=hvc0`'s own driver may be among the modules just loaded, in
/// which case the kernel found no console to hand `/init` and these fds
/// arrived closed — so they are claimed here, after the modules, rather
/// than inherited.
fn adopt_console() -> Result<(), String> {
    let console = openat(CWD, "/dev/console", OFlags::RDWR, Mode::empty())
        .map_err(|err| format!("could not open /dev/console: {err}"))?;
    for dup in [dup2_stdin, dup2_stdout, dup2_stderr] {
        dup(&console).map_err(|err| format!("could not adopt /dev/console: {err}"))?;
    }
    Ok(())
}

/// Whether a device node is there, which is the whole of what
/// [`plan::resolve_disks`] needs to know and the only impure part of the
/// probe.
fn device_present(device: &str) -> bool {
    stat(device).is_ok()
}

/// How long the disks are given to appear, and how often they are looked for.
///
/// Loading a block driver does not mean its disks exist yet: the driver
/// registers, the bus enumerates, and the nodes arrive when the scan reaches
/// them.  Under virtio that is fast enough to look like part of the module
/// load; under Hyper-V the storage driver's devices come over `VMBus` and are
/// scanned asynchronously, so a single look can lose a race it will win
/// milliseconds later.  Both are covered by waiting — briefly, because a
/// machine that really has no disks must still say so at second zero rather
/// than after a minute of silence.
const DISK_PATIENCE: std::time::Duration = std::time::Duration::from_secs(5);
const DISK_PULSE: std::time::Duration = std::time::Duration::from_millis(50);

/// The disks, once they exist — [`plan::resolve_disks`] against a devtmpfs that
/// may still be filling in.
///
/// The refusal on timeout is the resolver's own, naming every candidate it
/// looked for, so a machine with the wrong device model reads the same way
/// whether the nodes were late or absent.
///
/// # Errors
/// Returns [`plan::resolve_disks`]'s sentence if no candidate pair has appeared
/// within [`DISK_PATIENCE`].
fn await_disks() -> Result<plan::Disks, String> {
    let deadline = std::time::Instant::now() + DISK_PATIENCE;
    loop {
        match plan::resolve_disks(device_present) {
            Ok(disks) => return Ok(disks),
            Err(refusal) if std::time::Instant::now() >= deadline => return Err(refusal),
            Err(_) => std::thread::sleep(DISK_PULSE),
        }
    }
}

/// Format the session disk unconditionally: it arrives as a bare
/// zero-filled sparse file, so there is no existing filesystem to detect
/// and nothing to decide.
///
/// `device` is the session disk [`plan::resolve_disks`] chose, never a
/// constant: formatting the wrong half of a pair would erase the rootfs image
/// this boot is about to mount.
fn format_session(device: &str) -> Result<(), String> {
    let status = Command::new(plan::MKE2FS)
        .args(plan::FORMAT_SESSION_ARGS)
        .arg(device)
        .status()
        .map_err(|err| format!("could not run {}: {err}", plan::MKE2FS))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} on {device} exited {status}", plan::MKE2FS))
    }
}

fn make_dir(path: &str) -> Result<(), String> {
    if let Err(err) = mkdir(path, Mode::from_raw_mode(0o755))
        && err != Errno::EXIST
    {
        return Err(format!("could not create {path}: {err}"));
    }
    Ok(())
}

fn apply(m: &Mount) -> Result<(), String> {
    make_dir(m.target)?;
    mount(m.source, m.target, m.fstype, m.flags, m.data).map_err(|err| {
        format!(
            "could not mount {} ({}) at {}: {err}",
            m.source, m.fstype, m.target
        )
    })
}

/// Copy one binary from the initramfs onto the writable overlay upper,
/// executable, at the path the rest of the boot depends on.
fn install_binary(install: &Install) -> Result<(), String> {
    let dest = format!("{}{}", plan::NEWROOT, install.installed);
    if let Some(parent) = std::path::Path::new(&dest).parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
    }
    fs::copy(install.embedded, &dest)
        .map_err(|err| format!("could not install {} at {dest}: {err}", install.embedded))?;
    fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))
        .map_err(|err| format!("could not make {dest} executable: {err}"))
}

/// `switch_root`: reclaim the initramfs's own tmpfs, move the assembled
/// overlay to `/`, and exec `ral-daemon` as the new pid 1.
///
/// Linux's initial rootfs can never be unmounted — it is the one mount the
/// kernel will not let go of — so [`purge`] frees its memory the only way
/// available: deleting every file it holds, before the move makes it
/// unreachable for good.
fn switch_root() -> Result<Infallible, String> {
    let root_dev = stat("/")
        .map_err(|err| format!("could not stat /: {err}"))?
        .st_dev;
    let root = openat(CWD, "/", OFlags::DIRECTORY | OFlags::RDONLY, Mode::empty())
        .map_err(|err| format!("could not open /: {err}"))?;
    purge(root.as_fd(), root_dev)
        .map_err(|err| format!("could not reclaim the initramfs: {err}"))?;
    drop(root);

    // The cwd is the handle: a process's root pointer does not follow a
    // mount made over "/", so step into the new root first and let both the
    // move and the chroot name it as ".".
    chdir(plan::NEWROOT).map_err(|err| format!("could not enter {}: {err}", plan::NEWROOT))?;
    mount_move(".", "/").map_err(|err| format!("could not move {} to /: {err}", plan::NEWROOT))?;
    chroot(".").map_err(|err| format!("could not chroot into the new root: {err}"))?;
    chdir("/").map_err(|err| format!("could not chdir to /: {err}"))?;

    let err = Command::new(plan::RAL_DAEMON).exec();
    Err(format!("could not start {}: {err}", plan::RAL_DAEMON))
}

/// Delete everything under `dir` that lives on `dev` — the initramfs's own
/// filesystem — leaving anything mounted from elsewhere untouched. A
/// mountpoint is exactly a directory whose device differs from its
/// parent's, so nothing here needs to name the two disks or the overlay by
/// path to spare them.
fn purge(dir: BorrowedFd<'_>, dev: u64) -> rustix::io::Result<()> {
    for entry in Dir::read_from(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let info = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(info.st_mode) == FileType::Directory {
            if info.st_dev != dev {
                continue;
            }
            let sub = openat(
                dir,
                name,
                OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW,
                Mode::empty(),
            )?;
            purge(sub.as_fd(), dev)?;
            unlinkat(dir, name, AtFlags::REMOVEDIR)?;
        } else {
            unlinkat(dir, name, AtFlags::empty())?;
        }
    }
    Ok(())
}
