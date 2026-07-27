//! The host end of the guest's network.
//!
//! The guest's only network application endpoint is one explicit,
//! CONNECT-only proxy on the gateway's port 3128. The host admits an exact
//! list of public DNS names on port 443 and nothing else. TLS stays end to
//! end — the proxy never parses the tunnelled bytes — so this is a
//! **destination** policy, not an information-flow policy: an allowed host
//! receives whatever the guest sends it.
//!
//! Two honest limits. The gate does not enforce that TLS is what actually
//! flows through an open tunnel — a guest may speak anything to an allowed
//! origin's port 443. And an exact-name allowlist is really an allowlist of
//! the shared CDN infrastructure behind those names, since most allowed
//! hosts sit on shared edges.
//!
//! [`connect`] decides what a CONNECT request means, [`vet`] resolves and
//! pins a destination, [`stack`] is the `smoltcp` core that enforces both
//! at accept, and [`pipe`] is the byte-level seam between them and a
//! connection's worker thread.

pub mod connect;
pub mod device;
pub mod pipe;
pub mod stack;
pub mod vet;

pub use stack::Session;

/// Everything a session needs at start-up.
///
/// `gateway` is the interface's own address (also the guest's default
/// gateway — see `vm_manager::GUEST_LINK`); `dialer` is the seam tests use
/// to inject a dial that never opens an outbound socket.
pub struct Config {
    pub egress: exarch::egress::Egress,
    pub gateway: std::net::Ipv4Addr,
    pub dialer: std::sync::Arc<dyn vet::Dialer>,
}

/// Start a session over `wire` — the host's end of the net vsock link
/// `ral-daemon --pump` speaks `ral_daemon::packet`'s framing on. Returns
/// immediately; the [`Session`] is how a caller later stops it.
///
/// # Errors
/// Whatever starting either thread reports — see [`device::spawn`].
pub fn run<W: device::Wire>(wire: W, config: Config) -> std::io::Result<Session<W>> {
    stack::run(wire, config)
}
