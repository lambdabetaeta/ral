//! The guest's one network interface — a `tun` whose only peer is the host
//! stack on the other end of the net vsock wire (`crate::packet`,
//! `dev/docs/VM/SYNOD.md` §6).
//!
//! Same split as [`crate::sysctl`] and [`crate::mounts`]: [`plan`] turns the
//! host's [`boot::Net`] into an [`Interface`], a pure fact checkable without
//! a kernel; [`Interface::apply`] is the thin edge that performs it. That
//! split is why [`plan`] and [`Interface`] are not behind `target_os =
//! "linux"` the way the rest of this crate's syscall modules are — the
//! netmask and gateway arithmetic is worth running and testing on the
//! machine writing this code, not only inside the guest that will one day
//! run it, and `crate::packet` already sets the precedent of an ungated
//! module in a mostly-gated crate for exactly this reason.
//!
//! The same split governs the files the prologue carries: [`delivery`] turns
//! a [`crate::packet::Prologue`] into a list of [`Blob`]s, purely; each
//! [`Blob::apply`] is the thin edge that writes one.
//!
//! **ioctls only, no netlink**: the pinned `libc` carries `ifreq`,
//! `rtentry`, the `SIOC*` requests, `TUNSETIFF`, `IFF_TUN`, `IFF_NO_PI` and
//! `RTF_*` for both `musl` targets this daemon ships on, so reaching for
//! them keeps the crate's whole libc+rustix invariant — no netlink crate, no
//! second sockets abstraction — intact.

use std::net::Ipv4Addr;

use crate::boot;

/// The interface name every backend's console and the staging table expect
/// to see up.
const IFACE: &str = "ral0";

/// A `tun` interface, fully decided, before anything touches a kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interface {
    pub name: &'static str,
    pub address: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub gateway: Ipv4Addr,
}

/// The netmask a prefix length denotes, e.g. `/24` → `255.255.255.0`.
///
/// `prefix == 32` shifts by zero, which is fine; `prefix == 0` is
/// special-cased because a shift by 32 is not — `u32`'s shift operators
/// panic once the amount reaches the type's own width.
fn netmask(prefix: u8) -> Ipv4Addr {
    Ipv4Addr::from(if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    })
}

/// Decide the guest's `tun` interface from what the host put on the kernel
/// command line.
///
/// # Errors
/// A sentence naming the mistake: a prefix outside 0–32, a gateway equal to
/// the guest's own address, or a gateway outside `address/prefix` — the
/// last of these is also refused by [`boot::Boot::read`] before this ever
/// runs, but [`Interface`] takes a [`boot::Net`] on its own terms rather
/// than trusting that every caller went through the parser.
pub fn plan(net: &boot::Net) -> Result<Interface, String> {
    if net.prefix > 32 {
        return Err(format!(
            "the guest's network prefix, /{}, is not a valid IPv4 prefix — it must be 0 through 32.",
            net.prefix
        ));
    }
    if net.gateway == net.address {
        return Err(format!(
            "the gateway {} is the guest's own address; a gateway must be a different host on the \
             subnet.",
            net.gateway
        ));
    }
    let netmask = netmask(net.prefix);
    let network = |addr: Ipv4Addr| u32::from(addr) & u32::from(netmask);
    if network(net.gateway) != network(net.address) {
        return Err(format!(
            "the gateway {} is not inside {}/{}, the guest's own subnet — the guest could never \
             route to it.",
            net.gateway, net.address, net.prefix
        ));
    }
    Ok(Interface {
        name: IFACE,
        address: net.address,
        netmask,
        gateway: net.gateway,
    })
}

/// Where a [`Blob`]'s contents go once written: over the top of whatever was
/// there, or onto the end of it.
///
/// A sum rather than a bare bool because [`Blob::apply`] is the only place
/// that ever reads what `mode` means, and a name it can match on says why
/// each blob in [`delivery`] carries the one it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Truncate the file to exactly `contents`.
    Replace,
    /// Add `contents` to whatever the file already holds.
    Append,
}

/// One file the guest writes because the host's prologue said to, decided
/// before any of them touches a disk.
///
/// [`delivery`] is pure precisely because it stops here: whether
/// `/etc/ssl/certs/ca-certificates.crt` already holds anything is a fact
/// about the guest's filesystem, not the prologue, so appending to it is
/// [`Blob::apply`]'s job, not this struct's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blob {
    pub path: &'static str,
    pub contents: Vec<u8>,
    pub mode: Mode,
}

/// Turn the host's prologue into the files the guest must write before
/// anything downstream — TLS, DNS — can trust or resolve through it.
///
/// `resolv.conf` is replaced outright: it is the guest's whole resolver
/// configuration, and there is nothing already on disk worth keeping
/// alongside it. The CA PEM, when the host sent one, goes to two places for
/// two different readers: appended to the system bundle at
/// `/etc/ssl/certs/ca-certificates.crt` so every public root already there
/// still verifies whatever passes through the proxy untouched, and written
/// whole to `/usr/local/share/ca-certificates/synod-proxy.crt` — the
/// well-known holding pen `update-ca-certificates` would refresh the bundle
/// from, named separately so the proxy's own certificate can be told apart
/// from the public roots it was appended beside. An empty `ca_pem` means the
/// host has no proxy to be trusted for, not a mistake — [`Prologue::parse`]
/// already allows it, and this function answers with one blob instead of
/// three rather than three blobs that would each write nothing.
///
/// [`Prologue::parse`]: crate::packet::Prologue::parse
#[must_use]
pub fn delivery(prologue: &crate::packet::Prologue) -> Vec<Blob> {
    let mut blobs = vec![Blob {
        path: "/etc/resolv.conf",
        contents: prologue.resolv_conf.clone(),
        mode: Mode::Replace,
    }];
    if !prologue.ca_pem.is_empty() {
        blobs.push(Blob {
            path: "/etc/ssl/certs/ca-certificates.crt",
            contents: prologue.ca_pem.clone(),
            mode: Mode::Append,
        });
        blobs.push(Blob {
            path: "/usr/local/share/ca-certificates/synod-proxy.crt",
            contents: prologue.ca_pem.clone(),
            mode: Mode::Replace,
        });
    }
    blobs
}

#[cfg(target_os = "linux")]
mod apply {
    use std::ffi::CString;
    use std::io;
    use std::net::Ipv4Addr;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

    use super::{Blob, Interface, Mode};

    impl Blob {
        /// Write `contents` to `path`, truncating or appending as `mode`
        /// says, creating the file — and the directory holding it — if they
        /// are not there.
        ///
        /// The directory has to be created rather than assumed: a rootfs
        /// carrying no `ca-certificates` package has neither
        /// `/etc/ssl/certs` nor `/usr/local/share/ca-certificates`, and a
        /// guest that cannot be handed this session's certificate authority
        /// cannot speak to the proxy at all — so refusing the boot over a
        /// missing directory would turn a one-`mkdir` problem into no
        /// session.  Appending to a bundle that does not exist yet is
        /// likewise correct rather than a fallback: everything the guest
        /// reaches is intercepted, so our own authority is the only one it
        /// needs, and a bundle holding just that is a complete one.
        ///
        /// # Errors
        /// A sentence naming `path` and the reason the open or the write
        /// failed.
        pub fn apply(&self) -> Result<(), String> {
            use std::io::Write as _;
            if let Some(parent) = std::path::Path::new(self.path).parent() {
                std::fs::create_dir_all(parent).map_err(|err| {
                    format!("could not make the directory for {}: {err}", self.path)
                })?;
            }
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(self.mode == Mode::Replace)
                .append(self.mode == Mode::Append)
                .open(self.path)
                .and_then(|mut file| file.write_all(&self.contents))
                .map_err(|err| format!("could not write {}: {err}", self.path))
        }
    }

    /// Convert a `SIOC*`/`TUNSETIFF` request constant to `libc::Ioctl`.
    ///
    /// The pinned `libc` types `TUNSETIFF` as `Ioctl` directly but the
    /// `SIOC*` family as `c_ulong`, and `Ioctl` itself is `c_int` on musl
    /// and `c_ulong` on gnu — so an `as` cast here would silently truncate
    /// on one of the two ABIs this daemon actually ships on. `try_from`
    /// makes that width mismatch a compile-checked, panicking-on-mismatch
    /// conversion instead of a quiet one.
    fn code(request: libc::c_ulong) -> libc::Ioctl {
        libc::Ioctl::try_from(request)
            .expect("every SIOC* request this module issues is a small positive constant")
    }

    /// Issue one ioctl against `req`, the shape (`ifreq` or `rtentry`) the
    /// request expects. Callers pass `TUNSETIFF` as-is (it is already typed
    /// `Ioctl`) and every `SIOC*` constant through [`code`].
    fn ioctl<T>(fd: RawFd, request: libc::Ioctl, req: &mut T) -> io::Result<()> {
        // SAFETY: `req` is a live, fully initialised value of the type this
        // request reads and writes; every caller below hands it one.
        let rc = unsafe { libc::ioctl(fd, request, std::ptr::from_mut(req)) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// An `ifreq` naming `name` and nothing else — every field but
    /// `ifr_name` is a request-specific union member the caller fills in.
    ///
    /// # Panics
    /// If `name` is not ASCII — `libc::c_char` is signed on some targets
    /// this daemon builds for and unsigned on others, and every name this
    /// module ever passes in is one of its own `&'static str` constants.
    fn ifreq_named(name: &str) -> libc::ifreq {
        // SAFETY: `ifreq` is plain data (a name buffer and a union of
        // scalars/addresses); zero is a valid value for every field until
        // the caller sets the one this request uses.
        let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
        for (dst, src) in req.ifr_name.iter_mut().zip(name.bytes()) {
            *dst = libc::c_char::try_from(src).expect("interface names are ASCII");
        }
        req
    }

    /// A `sockaddr` carrying `addr` as `AF_INET` — the shape `SIOCSIFADDR`,
    /// `SIOCSIFNETMASK` and `SIOCADDRT` all read theirs in.
    fn sockaddr_in(addr: Ipv4Addr) -> libc::sockaddr {
        let sin = libc::sockaddr_in {
            sin_family: libc::sa_family_t::try_from(libc::AF_INET)
                .expect("AF_INET is a small positive address family"),
            sin_port: 0,
            sin_addr: libc::in_addr {
                s_addr: u32::from_ne_bytes(addr.octets()),
            },
            sin_zero: [0; 8],
        };
        // SAFETY: `sockaddr_in` and `sockaddr` are both 16 bytes on
        // Linux — a family tag followed by 14 bytes of payload — so an
        // `AF_INET` address is valid in either shape; this is the
        // reinterpretation every caller of a `sockaddr`-typed socket call
        // performs for an IPv4 address.
        unsafe { std::mem::transmute::<libc::sockaddr_in, libc::sockaddr>(sin) }
    }

    /// Open `/dev/net/tun`, or say in plain English why it is not there.
    ///
    /// `ENOENT` and `ENODEV` both collapse to the same three causes because
    /// nothing on this side of the syscall can tell them apart: the kernel
    /// was built without `CONFIG_TUN`, the `tun` module is missing from
    /// `/modules.order` in the boot image, or devtmpfs never created the
    /// node. Any of the three looks identical from here, so naming all
    /// three is the only honest refusal to give.
    fn open_tun() -> Result<std::fs::File, String> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/net/tun")
            .map_err(|err| match err.raw_os_error() {
                Some(errno) if errno == libc::ENOENT || errno == libc::ENODEV => format!(
                    "/dev/net/tun is not there ({err}). Three things cause that: the kernel was \
                     built without CONFIG_TUN, the tun module is missing from /modules.order in \
                     the boot image, or devtmpfs never created the node. Fix whichever it is and \
                     rebuild the boot image."
                ),
                _ => format!("could not open /dev/net/tun: {err}"),
            })
    }

    /// An `AF_INET`/`SOCK_DGRAM` socket, the handle every `SIOCSIF*` and
    /// `SIOCADDRT` request is issued against — the kernel addresses these
    /// by interface name, not by the tun descriptor itself.
    fn ip_socket() -> Result<OwnedFd, String> {
        // SAFETY: a plain `socket(2)` call; the returned descriptor is
        // adopted by `OwnedFd` immediately.
        let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if raw < 0 {
            return Err(format!(
                "could not open a socket to configure {}: {}",
                super::IFACE,
                io::Error::last_os_error()
            ));
        }
        // SAFETY: `raw` is a fresh descriptor this call owns.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }

    impl Interface {
        /// Create the `tun`, address it, and route the default gateway
        /// through it.
        ///
        /// Order: `TUNSETIFF` → `SIOCSIFADDR` → `SIOCSIFNETMASK` →
        /// `SIOCSIFMTU` → read-modify-write the flags to add `IFF_UP` →
        /// `SIOCADDRT` for the default route. The flags step reads before
        /// it writes rather than setting `IFF_UP` alone, because the kernel
        /// hands a freshly created `IFF_TUN` device `IFF_POINTOPOINT |
        /// IFF_NOARP` already set — an L3-only device has no hardware
        /// address to resolve, and clobbering those bits is how a default
        /// route through it would start missing ARP that was never meant
        /// to happen.
        ///
        /// # Errors
        /// A sentence naming the step and the errno the kernel gave; see
        /// [`open_tun`] for `/dev/net/tun`'s own three-cause refusal.
        ///
        /// # Panics
        /// Never in practice: the internal `expect`s are all compile-time
        /// constants (`IFF_TUN | IFF_NO_PI`, [`crate::packet::MTU`]) or an
        /// `IFF_UP` addition to flags the kernel itself just reported,
        /// none of which can fail to fit the C types they are converted
        /// into.
        pub fn apply(&self) -> Result<OwnedFd, String> {
            let tun = open_tun()?;
            let mut req = ifreq_named(self.name);
            // Writing a union field is safe; only reading one is not.
            req.ifr_ifru.ifru_flags = libc::c_short::try_from(libc::IFF_TUN | libc::IFF_NO_PI)
                .expect("IFF_TUN | IFF_NO_PI fits a c_short");
            // IFF_NO_PI is not optional: without it every packet on this
            // fd carries a 4-byte tun_pi header the wire format in
            // `crate::packet` knows nothing about, and the framing ships
            // garbage from the first packet on.
            ioctl(tun.as_raw_fd(), libc::TUNSETIFF, &mut req).map_err(|err| {
                format!("could not create the {} tun interface: {err}", self.name)
            })?;

            let sock = ip_socket()?;
            let mut req = ifreq_named(self.name);
            req.ifr_ifru.ifru_addr = sockaddr_in(self.address);
            ioctl(sock.as_raw_fd(), code(libc::SIOCSIFADDR), &mut req)
                .map_err(|err| format!("could not set {}'s address: {err}", self.name))?;

            req.ifr_ifru.ifru_netmask = sockaddr_in(self.netmask);
            ioctl(sock.as_raw_fd(), code(libc::SIOCSIFNETMASK), &mut req)
                .map_err(|err| format!("could not set {}'s netmask: {err}", self.name))?;

            req.ifr_ifru.ifru_mtu = libc::c_int::try_from(crate::packet::MTU)
                .expect("MTU is 1500, which fits a c_int");
            ioctl(sock.as_raw_fd(), code(libc::SIOCSIFMTU), &mut req)
                .map_err(|err| format!("could not set {}'s MTU: {err}", self.name))?;

            ioctl(sock.as_raw_fd(), code(libc::SIOCGIFFLAGS), &mut req)
                .map_err(|err| format!("could not read {}'s flags: {err}", self.name))?;
            // SAFETY: reading a union field, unlike writing one, is unsafe —
            // this is the request that just filled it in.
            let flags = unsafe { libc::c_int::from(req.ifr_ifru.ifru_flags) } | libc::IFF_UP;
            req.ifr_ifru.ifru_flags =
                libc::c_short::try_from(flags).expect("interface flags fit a c_short");
            ioctl(sock.as_raw_fd(), code(libc::SIOCSIFFLAGS), &mut req)
                .map_err(|err| format!("could not bring {} up: {err}", self.name))?;

            let dev_name = CString::new(self.name).expect("interface names carry no NUL byte");
            // SAFETY: `rtentry` is plain data; zero is a valid value for
            // every field until set below.
            let mut route: libc::rtentry = unsafe { std::mem::zeroed() };
            route.rt_dst = sockaddr_in(Ipv4Addr::UNSPECIFIED);
            route.rt_gateway = sockaddr_in(self.gateway);
            route.rt_genmask = sockaddr_in(Ipv4Addr::UNSPECIFIED);
            route.rt_flags = libc::RTF_UP | libc::RTF_GATEWAY;
            route.rt_dev = dev_name.as_ptr().cast_mut();
            ioctl(sock.as_raw_fd(), code(libc::SIOCADDRT), &mut route).map_err(|err| {
                format!(
                    "could not add {}'s default route via {}: {err}",
                    self.name, self.gateway
                )
            })?;

            Ok(tun.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(address: &str, prefix: u8, gateway: &str) -> boot::Net {
        boot::Net {
            port: 1730,
            address: address.parse().unwrap(),
            prefix,
            gateway: gateway.parse().unwrap(),
        }
    }

    #[test]
    fn a_slash_24_masks_the_last_octet() {
        assert_eq!(netmask(24), Ipv4Addr::new(255, 255, 255, 0));
    }

    #[test]
    fn a_slash_32_masks_nothing() {
        assert_eq!(netmask(32), Ipv4Addr::BROADCAST);
    }

    #[test]
    fn a_slash_0_masks_everything() {
        assert_eq!(netmask(0), Ipv4Addr::UNSPECIFIED);
    }

    #[test]
    fn a_slash_17_masks_across_the_octet_boundary() {
        assert_eq!(netmask(17), Ipv4Addr::new(255, 255, 128, 0));
    }

    #[test]
    fn a_gateway_on_the_subnet_is_accepted() {
        let interface = plan(&net("10.0.2.15", 24, "10.0.2.2")).unwrap();
        assert_eq!(interface.name, IFACE);
        assert_eq!(interface.address, Ipv4Addr::new(10, 0, 2, 15));
        assert_eq!(interface.netmask, Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(interface.gateway, Ipv4Addr::new(10, 0, 2, 2));
    }

    #[test]
    fn a_gateway_off_the_subnet_is_refused() {
        let err = plan(&net("10.0.2.15", 24, "10.0.3.2")).unwrap_err();
        assert!(err.contains("10.0.3.2"), "{err}");
        assert!(err.contains("route"), "{err}");
    }

    #[test]
    fn a_gateway_equal_to_the_address_is_refused() {
        let err = plan(&net("10.0.2.15", 24, "10.0.2.15")).unwrap_err();
        assert!(err.contains("own address"), "{err}");
    }

    #[test]
    fn a_prefix_over_32_is_refused() {
        let err = plan(&net("10.0.2.15", 33, "10.0.2.2")).unwrap_err();
        assert!(err.contains('/'), "{err}");
    }

    /// One row per shape a prologue can take: whether a CA PEM is present
    /// decides the whole list, since `resolv.conf` is unconditional.
    #[test]
    fn delivery_writes_resolv_conf_and_the_ca_only_when_one_arrived() {
        let resolv_conf = b"nameserver 10.0.2.2\n".to_vec();

        let without_ca = crate::packet::Prologue {
            resolv_conf: resolv_conf.clone(),
            ca_pem: Vec::new(),
        };
        assert_eq!(
            delivery(&without_ca),
            vec![Blob {
                path: "/etc/resolv.conf",
                contents: resolv_conf.clone(),
                mode: Mode::Replace,
            }]
        );

        let ca_pem = b"-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----\n".to_vec();
        let with_ca = crate::packet::Prologue {
            resolv_conf: resolv_conf.clone(),
            ca_pem: ca_pem.clone(),
        };
        assert_eq!(
            delivery(&with_ca),
            vec![
                Blob {
                    path: "/etc/resolv.conf",
                    contents: resolv_conf,
                    mode: Mode::Replace,
                },
                Blob {
                    path: "/etc/ssl/certs/ca-certificates.crt",
                    contents: ca_pem.clone(),
                    mode: Mode::Append,
                },
                Blob {
                    path: "/usr/local/share/ca-certificates/synod-proxy.crt",
                    contents: ca_pem,
                    mode: Mode::Replace,
                },
            ]
        );
    }
}
