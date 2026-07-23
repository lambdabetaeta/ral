//! A human-driven smoke boot for real boot artifacts.
//!
//! Nothing in this crate's own tests boots a real kernel — `Vz::boot`'s
//! contract is exercised by construction, not by a machine actually coming
//! up — so there was no way to point a person at the real thing. This is
//! that pointer: it boots the three artifacts through the `vz` backend and
//! holds the machine open until asked to stop, so a person can read
//! `ral-daemon`'s own console lines and judge the boot honestly.
//!
//! Requires a signed binary — Virtualization.framework refuses an
//! unentitled one. Run `dev/scripts/sign-virtualization.sh
//! target/debug/examples/boot-smoke` after every rebuild.
//!
//! Usage: `boot-smoke <kernel> <initramfs> <rootfs> <folder>`

#[cfg(target_os = "macos")]
fn main() {
    use vm_manager::vz::Vz;
    use vm_manager::{BootArtifact, Hypervisor, MachineSpec};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Ok([kernel, initramfs, rootfs, folder]) = <[String; 4]>::try_from(args) else {
        eprintln!("usage: boot-smoke <kernel> <initramfs> <rootfs> <folder>");
        std::process::exit(2);
    };
    let vz = Vz::new(
        BootArtifact {
            kernel: kernel.into(),
            initramfs: initramfs.into(),
            rootfs: rootfs.into(),
        },
        std::env::temp_dir(),
    );

    println!("booting via {}...", vz.name());
    let machine = match vz.boot(&MachineSpec::for_folder(folder)) {
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

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("boot-smoke drives vm_manager::vz, which builds on macOS only");
    std::process::exit(1);
}
