//! The net wire's pump: a dedicated process that shovels IPv4 packets
//! between the guest's `tun` and the host's net vsock socket, in each
//! direction, forever.
//!
//! ## Why a fourth argv arm and not a fourth crate or a subcommand
//!
//! The pump is `ral-daemon --pump`, dispatched in [`crate::serve`] before
//! the pid-1 check — not a third binary, and not an `exarch` subcommand.
//! A third binary would mean a fourth crate, a third entry in
//! `vm-image`'s `INSTALLS`, a third `cp` in `build-boot.sh`, and roughly
//! 400 KiB of cpio loaded into RAM every boot, to hold two `read`/`write`
//! loops that already live in the crate whose whole reason to exist is
//! `read`, `write`, and `fork`. An `exarch` subcommand would mean boot
//! plumbing that has to agree with a kernel command line and a fixed fd
//! layout living outside the one crate whose `Cargo.toml` *is* the
//! libc+rustix invariant this daemon depends on everywhere else. Neither
//! buys anything a fourth argv arm on the binary already running as pid 1
//! does not.
//!
//! [`spawn`] launches the pump by re-executing `/proc/self/exe` — no path
//! constant has to be kept in agreement between this crate and itself,
//! because procfs is already mounted by the time [`crate::init::serve`]
//! reaches it (`init.rs` mounts it before anything else).
//!
//! ## The fd contract
//!
//! [`spawn`] hands the child its `tun` and net-wire descriptors on fixed
//! numbers, [`TUN_FD`] and [`NET_FD`], the same way `crate::engine` hands
//! the engine its protocol socket on a fixed number. [`run`] adopts those
//! two fds and nothing else — no argument, no environment variable, no
//! third way to learn which descriptor is which.
//!
//! ## One writer per socket
//!
//! [`run`] splits into two threads, one per direction: `tun → net` reads a
//! raw IPv4 packet off the `tun` device and frames it onto the net socket;
//! `net → tun` reads a framed packet off the net socket and writes it raw
//! to `tun`. Each direction is the *only* thread that ever writes to its
//! destination, so two directions can never interleave two packets into
//! one socket — a property that held by construction rather than by a
//! lock.

use std::convert::Infallible;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::Command;
use std::sync::mpsc;
use std::{io, thread};

use rustix::process::Pid;

use crate::packet;
use crate::reap::Death;

/// The descriptor the pump reads and writes the guest's raw IPv4 packets on.
pub const TUN_FD: RawFd = 3;

/// The descriptor the pump reads and writes the net wire's framed packets on.
pub const NET_FD: RawFd = 4;

/// Launch the pump as a child of this process, handing it `tun` on
/// [`TUN_FD`] and `net` on [`NET_FD`].
///
/// Mirrors [`crate::engine::spawn`]'s `pre_exec`, with one addition: `tun`
/// and `net` arrive on whatever fd numbers the kernel happened to hand out,
/// which may already collide with [`TUN_FD`] or [`NET_FD`] — or with each
/// other's target. Dodging that is why each is first duplicated to a
/// descriptor `≥ 10` with `F_DUPFD` before either `dup2` runs: once both
/// sources sit safely above every fd this function ever names as a
/// destination, the two `dup2` calls below can run in either order without
/// one clobbering the other's source. The explicit `fcntl(F_SETFD, 0)` after
/// each — doubled, one per fd — is `engine.rs`'s own defence against a
/// `dup2` that turns out to be a same-fd no-op and so leaves `CLOEXEC` set.
///
/// # Errors
/// Returns a sentence naming this binary when it cannot be re-executed.
pub fn spawn(tun: &OwnedFd, net: &OwnedFd) -> Result<Pid, String> {
    let (tun, net) = (tun.as_raw_fd(), net.as_raw_fd());
    let mut cmd = Command::new("/proc/self/exe");
    cmd.arg("--pump");
    cmd.env_clear();
    // SAFETY: the closure runs between `fork` and `exec` and calls only
    // async-signal-safe syscalls — `fcntl`, `dup2` — with no allocation and
    // no locking.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            let dup_high = |fd: RawFd| -> io::Result<RawFd> {
                match libc::fcntl(fd, libc::F_DUPFD, 10) {
                    n if n < 0 => Err(io::Error::last_os_error()),
                    n => Ok(n),
                }
            };
            let (tun_high, net_high) = (dup_high(tun)?, dup_high(net)?);
            for (from, to) in [(tun_high, TUN_FD), (net_high, NET_FD)] {
                if libc::dup2(from, to) < 0 {
                    return Err(io::Error::last_os_error());
                }
                libc::close(from);
                if libc::fcntl(to, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .map_err(|err| format!("could not start the net pump at /proc/self/exe: {err}"))?;
    Ok(Pid::from_child(&child))
}

/// Be the pump: adopt [`TUN_FD`] and [`NET_FD`] and shovel packets between
/// them until one side fails.
///
/// Never returns normally — the only way out is the [`String`] naming
/// whichever direction broke first, which is also, by the reasoning in
/// [`epitaph`], the whole session ending.
///
/// # Errors
/// Returns a sentence naming the direction and the I/O failure that ended
/// it.
pub fn run() -> Result<Infallible, String> {
    // SAFETY: `spawn` places exactly these two descriptors here before
    // `exec`, and this function's only caller is the `--pump` arm of
    // `crate::serve`, which never runs except as that child.
    let tun = unsafe { File::from_raw_fd(TUN_FD) };
    let net = unsafe { File::from_raw_fd(NET_FD) };
    // Each direction needs both descriptors — one to read, one to write —
    // so each gets its own duplicate rather than sharing the originals: two
    // `File`s over the same open file description, each closed by whichever
    // thread's loop ends first, without disturbing the other's copy.
    let (tun_for_net, net_for_net) = (
        tun.try_clone()
            .map_err(|err| format!("could not duplicate the tun descriptor: {err}"))?,
        net.try_clone()
            .map_err(|err| format!("could not duplicate the net-wire descriptor: {err}"))?,
    );

    let (tx, rx) = mpsc::channel();
    let to_net = tx.clone();
    thread::spawn(move || {
        let Err(err) = tun_to_net(tun_for_net, net_for_net);
        let _ = to_net.send(format!("the tun\u{2192}net direction {err}"));
    });
    thread::spawn(move || {
        let Err(err) = net_to_tun(net, tun);
        let _ = tx.send(format!("the net\u{2192}tun direction {err}"));
    });
    Err(rx.recv().unwrap_or_else(|_| {
        "both pump directions ended without saying why, which should not happen".to_string()
    }))
}

/// Read raw IPv4 packets off `tun` and frame each onto `net`, forever.
fn tun_to_net(mut tun: File, mut net: File) -> Result<Infallible, String> {
    let mut buf = [0u8; packet::MTU];
    loop {
        let n = tun
            .read(&mut buf)
            .map_err(|err| format!("could not read the tun device: {err}"))?;
        if n == 0 {
            return Err("saw the tun device close".to_string());
        }
        packet::write_frame(&mut net, &buf[..n])
            .map_err(|err| format!("could not write the net wire: {err}"))?;
    }
}

/// Read framed packets off `net` and write each raw to `tun`, forever.
fn net_to_tun(mut net: File, mut tun: File) -> Result<Infallible, String> {
    loop {
        let frame = packet::read_frame(&mut net)
            .map_err(|err| format!("could not read the net wire: {err}"))?;
        tun.write_all(&frame)
            .map_err(|err| format!("could not write the tun device: {err}"))?;
    }
}

/// What the pump's death means, said plainly enough for a host-side log.
///
/// The pump gets the same no-restart policy [`crate::engine::epitaph`]
/// states for the engine, for the same underlying reason plus one of its
/// own: both host backends accept the net wire exactly once, by
/// construction, so there is nothing to restart *into* — a second pump
/// would have no socket to inherit. Degrading to no-network instead of
/// ending the session was considered and rejected: a session that carries
/// on silently unable to reach anything it could reach a moment ago is
/// exactly the "still running and quietly wrong" failure
/// `crate::engine`'s own module docs already refuse to accept for the
/// engine, and there is no principled reason to accept it here.
pub fn epitaph(death: Death) -> String {
    format!("the network pump {death}; the session cannot be resumed in place, powering the machine off")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pump's death reads as a sentence, the same register as the
    /// engine's — this is the one thing about the pump worth testing
    /// without a kernel, since [`spawn`] and [`run`] are both a thin edge.
    #[test]
    fn the_epitaph_names_the_death_and_the_shutdown() {
        let text = epitaph(Death::Signalled(9));
        assert!(text.contains("killed by signal 9"), "{text}");
        assert!(text.contains("cannot be resumed"), "{text}");
    }
}
