//! A human-driven smoke boot for real boot artifacts.
//!
//! Nothing in this crate's own tests boots a real kernel — a backend's
//! contract is exercised by construction, not by a machine actually coming
//! up — so there was no way to point a person at the real thing. This is
//! that pointer: it boots the three artifacts through whichever backend this
//! platform has and holds the machine open until asked to stop, so a person
//! can read `ral-daemon`'s own console lines and judge the boot honestly.
//!
//! # What each platform asks of you first
//!
//! - **macOS** — a signed binary, since Virtualization.framework refuses an
//!   unentitled one. Run `dev/scripts/sign-virtualization.sh
//!   target/debug/examples/boot-smoke` after every rebuild.
//! - **Windows** — an account the compute service serves: an administrator, or
//!   a member of the *Hyper-V Administrators* group. Nothing needs signing and
//!   the process itself is unprivileged; the group membership is what the
//!   service checks. Without it the boot refuses by naming the group, which is
//!   itself a useful thing to have seen.
//!
//! Usage: `boot-smoke <kernel> <initramfs> <rootfs> <folder>`

use vm_manager::{BootArtifact, Hypervisor, MachineSpec};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Ok([kernel, initramfs, rootfs, folder]) = <[String; 4]>::try_from(args) else {
        eprintln!("usage: boot-smoke <kernel> <initramfs> <rootfs> <folder>");
        std::process::exit(2);
    };
    let artifact = BootArtifact {
        kernel: kernel.into(),
        initramfs: initramfs.into(),
        rootfs: rootfs.into(),
    };
    let Some(hypervisor) = backend(artifact) else {
        eprintln!("this platform has no virtual-machine backend in vm-manager");
        std::process::exit(1);
    };

    println!("booting via {}...", hypervisor.name());
    let machine = match hypervisor.boot(&MachineSpec::for_folder(folder)) {
        Ok(machine) => machine,
        Err(err) => {
            eprintln!("boot failed: {err}");
            std::process::exit(1);
        }
    };
    println!("booted: the agent can reach the granted folder and nothing else on this computer");
    println!("ral-daemon's own console lines print above as the guest runs.");
    println!("press enter to shut the machine down");
    let _ = std::io::stdin().read_line(&mut String::new());

    match machine.shutdown() {
        Ok(()) => println!("stopped cleanly"),
        Err(err) => {
            eprintln!("shutdown failed: {err}");
            std::process::exit(1);
        }
    }
}

/// The backend this platform has, constructed directly rather than through
/// [`vm_manager::detect`] — an example already holds the boot media `detect`
/// exists to look for, and asking it again would only add a way to be refused.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the `Option` is the seam's shape, not this arm's: a platform with a backend always \
              answers `Some`, and the one without answers `None` from the third arm below"
)]
#[cfg(target_os = "macos")]
fn backend(artifact: BootArtifact) -> Option<Box<dyn Hypervisor>> {
    Some(Box::new(vm_manager::vz::Vz::new(
        artifact,
        std::env::temp_dir(),
    )))
}

/// The backend this platform has — see the macOS twin for why it is built
/// directly. The cache is the real one synod would use rather than a temporary
/// directory: the rootfs wrap it may have to write is worth keeping between
/// runs.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the `Option` is the seam's shape, not this arm's: a platform with a backend always \
              answers `Some`, and the one without answers `None` from the third arm below"
)]
#[cfg(windows)]
fn backend(artifact: BootArtifact) -> Option<Box<dyn Hypervisor>> {
    use vm_manager::hcs::Hyperv;
    Some(Box::new(Hyperv::new(artifact, Hyperv::default_cache())))
}

/// No backend here, and the example says so at run time rather than failing to
/// build — the same courtesy [`vm_manager::detect`] extends.
#[cfg(not(any(target_os = "macos", windows)))]
fn backend(artifact: BootArtifact) -> Option<Box<dyn Hypervisor>> {
    drop(artifact);
    None
}
