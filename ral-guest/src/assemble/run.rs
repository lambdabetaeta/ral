//! Load drivers before probing disks; assemble and enter the root before supervision.

use std::fs;
use std::mem::ManuallyDrop;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::{Duration, Instant};

use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, openat, stat, statat, unlinkat};
use rustix::io::Errno;
use rustix::mount::mount_move;
use rustix::process::{chdir, chroot};
use rustix::stdio::{dup2_stderr, dup2_stdin, dup2_stdout};
use rustix::system::finit_module;

use super::plan;
use crate::mount;

pub(crate) fn assemble() -> Result<(), String> {
    mount::dev().apply()?;
    load_modules()?;
    adopt_console()?;
    // devtmpfs and drivers must precede probing; console adoption makes a refusal visible.
    let disks = await_disks()?;
    format_session(disks.session)?;
    for disk in plan::disk_mounts(disks) {
        disk.apply()?;
    }
    for dir in [plan::UPPER, plan::WORKDIR] {
        mount::ensure_dir(dir)?;
    }
    plan::overlay().apply()?;
    install_engine()?;
    install_legal_files()?;
    switch_root()
}

/// Load modules to a fixpoint; manifest order is only a hint, and an absent manifest is allowed.
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
                // EEXIST means the required module is already loaded.
                Ok(()) | Err(Errno::EXIST) => {}
                Err(err) => {
                    errors.push(format!("{name} ({err})"));
                    stuck.push(*name);
                }
            }
        }
        // Stop at the fixpoint: no loaded module means no new dependency can resolve next pass.
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

/// Adopt the console after modules: its driver may have been absent when the kernel started PID 1.
fn adopt_console() -> Result<(), String> {
    let console = openat(CWD, "/dev/console", OFlags::RDWR, Mode::empty())
        .map_err(|err| format!("could not open /dev/console: {err}"))?;
    adopt_stdio(console).map_err(|err| format!("could not adopt /dev/console: {err}"))
}

fn adopt_stdio(console: OwnedFd) -> Result<(), Errno> {
    let standard;
    let source = if matches!(console.as_raw_fd(), 0..=2) {
        // A standard source must survive even a later duplication failure.
        standard = ManuallyDrop::new(console);
        standard.as_fd()
    } else {
        console.as_fd()
    };
    for dup in [dup2_stdin, dup2_stdout, dup2_stderr] {
        dup(source)?;
    }
    Ok(())
}

fn device_present(device: &str) -> bool {
    stat(device).is_ok()
}

/// Block-device enumeration is asynchronous, especially over Hyper-V `VMBus`.
const DISK_PATIENCE: Duration = Duration::from_secs(5);
const DISK_PULSE: Duration = Duration::from_millis(50);

fn await_disks() -> Result<plan::Disks, String> {
    let deadline = Instant::now() + DISK_PATIENCE;
    loop {
        match plan::resolve_disks(device_present) {
            Ok(disks) => return Ok(disks),
            Err(refusal) if Instant::now() >= deadline => return Err(refusal),
            Err(_) => std::thread::sleep(DISK_PULSE),
        }
    }
}

/// Always format the probed session device; never format the rootfs half of the pair.
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

fn install_engine() -> Result<(), String> {
    let dest = format!("{}{}", plan::NEWROOT, guest_abi::boot::DEFAULT_ENGINE);
    if let Some(parent) = std::path::Path::new(&dest).parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
    }
    fs::copy(plan::ENGINE_EMBEDDED, &dest).map_err(|err| {
        format!(
            "could not install {} at {dest}: {err}",
            plan::ENGINE_EMBEDDED
        )
    })?;
    fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))
        .map_err(|err| format!("could not make {dest} executable: {err}"))
}

fn install_legal_files() -> Result<(), String> {
    const SOURCE: &str = "/usr/share/doc/exarch";
    let dest = format!("{}{SOURCE}", plan::NEWROOT);
    fs::create_dir_all(&dest)
        .map_err(|err| format!("could not create guest legal-file directory {dest}: {err}"))?;
    for name in ["LICENSE-APACHE", "LICENSE-MIT", "NOTICE", "LICENSE-EDTUI"] {
        let source = format!("{SOURCE}/{name}");
        let target = format!("{dest}/{name}");
        fs::copy(&source, &target).map_err(|err| {
            format!(
                "could not preserve {source} at {target} before switch_root: {err}; \
                 vm-image/build-boot.sh must include all four exarch legal files in the initramfs"
            )
        })?;
    }
    Ok(())
}

fn switch_root() -> Result<(), String> {
    let root_dev = stat("/")
        .map_err(|err| format!("could not stat /: {err}"))?
        .st_dev;
    let root = openat(CWD, "/", OFlags::DIRECTORY | OFlags::RDONLY, Mode::empty())
        .map_err(|err| format!("could not open /: {err}"))?;
    purge(root.as_fd(), root_dev)
        .map_err(|err| format!("could not reclaim the initramfs: {err}"))?;
    drop(root);

    // The root pointer does not follow an overmount; enter the new root before move and chroot.
    chdir(plan::NEWROOT).map_err(|err| format!("could not enter {}: {err}", plan::NEWROOT))?;
    mount_move(".", "/").map_err(|err| format!("could not move {} to /: {err}", plan::NEWROOT))?;
    chroot(".").map_err(|err| format!("could not chroot into the new root: {err}"))?;
    chdir("/").map_err(|err| format!("could not chdir to /: {err}"))
}

/// Purge only the initramfs device without following links; preserve mounted filesystems.
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fs::fstat;
    use rustix::stdio::{stderr, stdin, stdout};

    #[test]
    fn console_adoption_preserves_standard_descriptors() {
        const CASE: &str = "RAL_GUEST_STDIO_CASE";
        // Not 0, which a filter matching no test also exits with.
        const ADOPTED: i32 = 42;
        if let Ok(mask) = std::env::var(CASE) {
            let mask: u8 = mask.parse().unwrap();
            for fd in 0..=2 {
                if mask & (1 << fd) != 0 {
                    // This subprocess alone owns its standard descriptors.
                    unsafe { rustix::io::close(fd) };
                }
            }
            let console = openat(CWD, "/dev/null", OFlags::RDWR, Mode::empty()).unwrap();
            let source = console.as_raw_fd();
            let expected = fstat(&console).unwrap();
            adopt_stdio(console).unwrap();
            for fd in [stdin(), stdout(), stderr()] {
                let actual = fstat(fd).unwrap();
                assert_eq!(actual.st_dev, expected.st_dev);
                assert_eq!(actual.st_ino, expected.st_ino);
                assert_eq!(actual.st_rdev, expected.st_rdev);
            }
            if source > 2 {
                assert_eq!(
                    stat(format!("/proc/self/fd/{source}")).unwrap_err(),
                    Errno::NOENT
                );
            }
            let next = openat(CWD, "/dev/null", OFlags::RDWR, Mode::empty()).unwrap();
            assert!(next.as_raw_fd() > 2);
            std::process::exit(ADOPTED);
        }
        for mask in 0..8 {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "assemble::run::tests::console_adoption_preserves_standard_descriptors",
                    "--nocapture",
                ])
                .env(CASE, mask.to_string())
                .status()
                .unwrap();
            assert_eq!(
                status.code(),
                Some(ADOPTED),
                "closed-descriptor mask {mask}"
            );
        }
    }
}
