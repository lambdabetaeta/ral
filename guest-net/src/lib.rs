//! The host end of the guest's network: a user-mode TCP/IP stack, and the
//! four gates the guest's traffic has to pass through to leave this process.
//!
//! `dev/docs/VM/SYNOD.md` §6 gives the shape. Neither hypervisor backend
//! configures a network adapter; the guest's only device is a `tun` whose
//! sole peer is this process, reached over a third vsock port. So every
//! packet the guest emits arrives here as bytes, and nothing it can do to
//! its own kernel routes around that.
//!
//! The four gates, each closing one bypass:
//!
//! 1. **DNS is answered here.** A name the policy does not admit gets
//!    `NXDOMAIN`; an admitted one gets an address minted for it.
//! 2. **TCP is terminated in user mode**, and the list is checked *at
//!    accept*, by host and port. This is the gate that closes raw IP: an
//!    address DNS never issued belongs to no name, cannot be classified,
//!    and is refused before a byte is read.
//! 3. **80 and 443 are redirected to an intercepting proxy**, which is what
//!    makes a grant a *verb* on a host — `GET files.pythonhosted.org`
//!    rather than `files.pythonhosted.org`. Bytes can only be judged where
//!    they are in the clear.
//! 4. **Everything else is refused** by the absence of a listener, so
//!    smoltcp answers RST or ICMP port-unreachable rather than a black hole.
//!
//! The residual is honest and recorded in `docs/ral-wiki/design/egress.md`:
//! an allowed `GET` still carries a query string, so the ledger is
//! load-bearing rather than decorative.

pub mod ca;
pub mod device;
pub mod dns;
pub mod names;
pub mod pipe;
pub mod proxy;
pub mod refusal;
pub mod stack;
pub mod upstream;

pub use stack::{Accepted, Handler, Session};

/// Everything a guest-net session needs at start-up.
///
/// The shared policy, ledger and (HTTP) rate budget every fleet process
/// already carries as [`exarch::egress::Egress`]; the interface's own
/// address, which is also the address the guest's `resolv.conf` names as
/// its nameserver (Track C's prologue and this crate's DNS listener must
/// agree on it, so it travels as one field rather than two constants kept
/// in sync by hand); the DNS rate cap, a second budget distinct from
/// `egress.limiter`'s HTTP one, since a name lookup and a fetch are
/// different amplification risks; and the [`Handler`] that turns an
/// [`Accepted`] connection into an intercepted HTTP request — `synod`'s to
/// supply, this crate never looks inside it.
pub struct Config {
    pub egress: exarch::egress::Egress,
    pub gateway: std::net::Ipv4Addr,
    pub dns_rate_per_minute: u32,
    pub handler: Handler,
}

/// Start a guest-net session over `wire` — the host's end of the net vsock
/// link `ral-daemon --pump` speaks `ral_daemon::packet`'s framing on.
///
/// Spawns the reader thread and the core thread and returns immediately;
/// the returned [`Session`] is how a caller later stops it.
///
/// # Errors
/// Whatever starting either thread reports — see [`device::spawn`].
pub fn run<W: device::Wire>(wire: W, config: Config) -> std::io::Result<Session<W>> {
    stack::run(wire, config)
}
