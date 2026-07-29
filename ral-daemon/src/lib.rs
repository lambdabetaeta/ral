//! ral-daemon — PID 1 inside a synod or exarch guest.
//!
//! One line of job: mount, clock sync, spawn/supervise the engine, reap
//! zombies, console→host log — and never in the protocol's data path.  Its
//! boringness is the design.  It holds no ral semantics and no authority
//! policy; it decides nothing about what the agent may do, only that there is
//! a Linux userland for the engine to run in and somebody to bury the dead.
//!
//! ## The boot narrative
//!
//! 1. **Refuse politely unless we are init.**  Nothing below is survivable on
//!    a machine that is not a guest, so the check comes before the first
//!    syscall that changes anything.
//! 2. **Mount `/proc`.**  The one mount that cannot wait for a decision: the
//!    configuration is read from `/proc/cmdline`.
//! 3. **Read the kernel command line** into a [`boot::Boot`] — the only
//!    thing the host has told the guest at this point.
//! 4. **Report what `/` actually is.**  The root — a read-only rootfs image
//!    under a per-session upper layer — is assembled by the stage before
//!    this one (see `mounts`); the daemon names what it was handed rather
//!    than assuming.
//! 5. **Take the host's time.**  The guest has no real-time clock worth
//!    trusting; the host wrote its own on the command line and the daemon
//!    applies it to `CLOCK_REALTIME`.  It is set once, at boot: there is no
//!    ongoing discipline, so a very long session drifts by whatever the
//!    hypervisor's timekeeping drifts by.  When that starts to matter the
//!    place to fix it is a periodic correction over the protocol, not a
//!    second clock authority down here.
//! 6. **Mount everything else**, in the order `mounts::plan` fixes — which,
//!    where the host runs a hypervisor with no virtiofs, includes dialling
//!    the host for the workspace's own 9p transport.  Whether it does is a
//!    fact the command line carried, not one the guest guesses at.
//! 7. **Apply the guest-wide sysctls** (`sysctl::plan`) the engine's spawn
//!    jail depends on — user namespaces off — before any external code gets
//!    a chance to run.
//! 8. **Listen for the end.**  PID 1 has no default signal dispositions, so
//!    the handlers for "the host wants this machine off" are installed by
//!    hand, before there is a child that could outlive them.
//! 9. **Dial the host's control plane**, but do not hand it to the engine
//!    yet — the engine is the *last* thing this daemon starts.
//! 10. **Bring the network up, when this boot has one** (`boot::Boot::net`):
//!     dial the host's net wire — packet framing begins on it immediately,
//!     with no prologue ahead of it — create and address the `tun`
//!     (`net::plan`, `net::Interface::apply`), and start the pump
//!     (`pump::spawn`) that will shovel packets across it.
//! 11. **Start the engine**, now that its network — if this boot has one —
//!     is already live, and hand it the control-plane connection from step 9
//!     as fd 3.
//! 12. **Wait.**  Forever, until the engine or the pump dies, or the host
//!     says stop — either child's death ends the session; see
//!     `engine::epitaph` and `pump::epitaph`.
//!
//! ## Console → host log
//!
//! There is no logging machinery here, and that is deliberate: the kernel
//! opens `/dev/console` as init's standard input, output, and error, and the
//! hypervisor pipes that console to the host's session log.  So the daemon's
//! `eprintln!`s *are* the host log, the engine inherits the same descriptors
//! and joins it, and the whole feature costs a `ral-daemon:` prefix so the
//! two voices can be told apart.  It assumes the VM backend puts a working
//! `console=` on the kernel command line — `console=hvc0` for a
//! Virtualization.framework serial port.
//!
//! ## What can and cannot be checked here
//!
//! Every *decision* in this crate — which filesystems, in what order, with
//! what flags; how the engine's command line, environment, and descriptors
//! are assembled; how a wait result is classified; what a death means — is a
//! pure function over data, and is unit-tested.  The syscalls that carry
//! them out are a thin edge that can only be exercised inside a real guest,
//! and nothing here pretends otherwise.

#![allow(
    clippy::disallowed_methods,
    reason = "ral-daemon is a guest's init, not the ral shell; the clippy.toml doors govern ral-core's Shell path/cwd/fs discipline, and PID 1's whole job is the syscalls they route"
)]

pub mod boot;
pub mod net;
pub mod packet;

#[cfg(target_os = "linux")]
pub mod engine;
#[cfg(target_os = "linux")]
pub mod mounts;
#[cfg(target_os = "linux")]
pub mod pump;
#[cfg(target_os = "linux")]
pub mod reap;
#[cfg(target_os = "linux")]
pub mod sysctl;

#[cfg(target_os = "linux")]
mod init;
#[cfg(target_os = "linux")]
mod vsock;

#[cfg(target_os = "linux")]
pub use init::serve;

/// Refuse, on a machine that has no guest to be the init of.
///
/// The crate builds everywhere so the VM backend — written on macOS — can
/// link the [`boot`] contract and test it; the daemon itself exists only in
/// a Linux guest.
///
/// # Errors
/// Always.
#[cfg(not(target_os = "linux"))]
pub fn serve() -> Result<std::convert::Infallible, String> {
    Err("ral-daemon is the init process of a Linux guest; this is not one.".to_string())
}
