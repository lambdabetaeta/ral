//! ral-initramfs — the program that runs as `/init` before `ral-daemon`
//! exists to run as anything.
//!
//! `ral-daemon/src/mounts.rs` already says what it needs from the stage
//! before it: a final root, already assembled, before `switch_root` hands
//! off to it — "the initramfs, which mounts the two images, stacks the
//! overlay, and `switch_root`s into it." This crate *is* that stage, and
//! its job is exactly as fixed as that sentence: two known virtio-blk disks
//! (`plan::ROOTFS_DEVICE`, `plan::SESSION_DEVICE` — vm-manager's `vz.rs`
//! attaches them in this order and never another), one overlay, one
//! binary-install step, one `switch_root`.
//!
//! `rootfs.img` carries no ral binaries at all, and `ral-daemon`'s own
//! `DEFAULT_ENGINE` names a path inside the tree `switch_root` moves into —
//! so this program is also the mechanism behind `boot.img`'s claim to carry
//! "kernel + ral-daemon + ral engine as one artifact": neither binary is
//! baked into the read-only rootfs, they are installed fresh onto the
//! writable overlay upper on every boot (see [`plan::INSTALLS`]).
//!
//! Following `mounts.rs`'s own split: [`plan`] is what to do, named as data
//! and unit-tested without a virtual machine; [`init`] is the syscall
//! sequence that carries it out, exercisable only inside a real guest.

#![allow(
    clippy::disallowed_methods,
    reason = "ral-initramfs is the guest's /init, run before ral-daemon or any userland exists; \
              the io-door doors govern ral-core's Shell path/cwd/fs discipline, and this \
              program's whole job is the syscalls and process spawns they route"
)]

#[cfg(target_os = "linux")]
pub mod plan;

#[cfg(target_os = "linux")]
mod init;

#[cfg(target_os = "linux")]
pub use init::run;

/// Refuse, on a machine that has no initramfs to be the `/init` of.
///
/// The crate builds everywhere so the workspace checks on any host; the
/// program — and its Linux-only plan — exists only for a guest's initial
/// ramdisk.
///
/// # Errors
/// Always.
#[cfg(not(target_os = "linux"))]
pub fn run() -> Result<std::convert::Infallible, String> {
    Err("ral-initramfs is the /init of a ral guest's initial ramdisk; this is not one.".into())
}
