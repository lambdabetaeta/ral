//! The tun's only peer is the host stack over the net vsock wire.
//! The pinned libc supplies every ioctl for both musl targets; no netlink is needed.

use std::ffi::CString;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use guest_abi::boot;

const IFACE: &str = "ral0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Interface {
    pub(crate) address: Ipv4Addr,
    pub(crate) netmask: Ipv4Addr,
    pub(crate) gateway: Ipv4Addr,
}

pub(crate) fn plan(net: &boot::Net) -> Interface {
    let link = net.link();
    Interface {
        address: link.address,
        netmask: net.netmask(),
        gateway: link.gateway,
    }
}

/// `libc::Ioctl` is `c_int` on musl and `c_ulong` on GNU; check the SIOC* width conversion.
fn code(request: libc::c_ulong) -> libc::Ioctl {
    libc::Ioctl::try_from(request)
        .expect("every SIOC* request this module issues is a small positive constant")
}

/// req must have the initialized shape (ifreq or rtentry) required by request.
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

fn ifreq_named(name: &str) -> libc::ifreq {
    // SAFETY: ifreq is plain data; zero is valid for every field before the caller fills it.
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in req.ifr_name.iter_mut().zip(name.bytes()) {
        *dst = libc::c_char::try_from(src).expect("interface names are ASCII");
    }
    req
}

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
    // SAFETY: on Linux both types are 16 bytes; the AF_INET tag identifies the IPv4 payload.
    unsafe { std::mem::transmute::<libc::sockaddr_in, libc::sockaddr>(sin) }
}

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

/// Interface configuration ioctls address names through this socket, not the tun fd.
fn ip_socket() -> Result<OwnedFd, String> {
    // SAFETY: a plain `socket(2)` call; the returned descriptor is
    // adopted by `OwnedFd` immediately.
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(format!(
            "could not open a socket to configure {}: {}",
            IFACE,
            io::Error::last_os_error()
        ));
    }
    // SAFETY: `raw` is a fresh descriptor this call owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

impl Interface {
    /// Preserve `IFF_POINTOPOINT` and `IFF_NOARP` when adding `IFF_UP` to avoid spurious ARP.
    pub(crate) fn apply(&self) -> Result<OwnedFd, String> {
        let tun = open_tun()?;
        let mut req = ifreq_named(IFACE);
        req.ifr_ifru.ifru_flags = libc::c_short::try_from(libc::IFF_TUN | libc::IFF_NO_PI)
            .expect("IFF_TUN | IFF_NO_PI fits a c_short");
        // IFF_NO_PI keeps the four-byte tun_pi header out of the raw-IP wire format.
        ioctl(tun.as_raw_fd(), libc::TUNSETIFF, &mut req)
            .map_err(|err| format!("could not create the {IFACE} tun interface: {err}"))?;

        let sock = ip_socket()?;
        let mut req = ifreq_named(IFACE);
        req.ifr_ifru.ifru_addr = sockaddr_in(self.address);
        ioctl(sock.as_raw_fd(), code(libc::SIOCSIFADDR), &mut req)
            .map_err(|err| format!("could not set {IFACE}'s address: {err}"))?;

        req.ifr_ifru.ifru_netmask = sockaddr_in(self.netmask);
        ioctl(sock.as_raw_fd(), code(libc::SIOCSIFNETMASK), &mut req)
            .map_err(|err| format!("could not set {IFACE}'s netmask: {err}"))?;

        req.ifr_ifru.ifru_mtu =
            libc::c_int::try_from(guest_abi::link::MTU).expect("MTU is 1500, which fits a c_int");
        ioctl(sock.as_raw_fd(), code(libc::SIOCSIFMTU), &mut req)
            .map_err(|err| format!("could not set {IFACE}'s MTU: {err}"))?;

        ioctl(sock.as_raw_fd(), code(libc::SIOCGIFFLAGS), &mut req)
            .map_err(|err| format!("could not read {IFACE}'s flags: {err}"))?;
        // SAFETY: SIOCGIFFLAGS just initialized this union field.
        let flags = unsafe { libc::c_int::from(req.ifr_ifru.ifru_flags) } | libc::IFF_UP;
        req.ifr_ifru.ifru_flags =
            libc::c_short::try_from(flags).expect("interface flags fit a c_short");
        ioctl(sock.as_raw_fd(), code(libc::SIOCSIFFLAGS), &mut req)
            .map_err(|err| format!("could not bring {IFACE} up: {err}"))?;

        let dev_name = CString::new(IFACE).expect("interface names carry no NUL byte");
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
                "could not add {IFACE}'s default route via {}: {err}",
                self.gateway
            )
        })?;

        Ok(tun.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(address: &str, prefix: u8, gateway: &str) -> boot::Net {
        boot::Net::new(
            1730,
            guest_abi::link::Link {
                address: address.parse().unwrap(),
                prefix,
                gateway: gateway.parse().unwrap(),
            },
        )
        .unwrap()
    }

    #[test]
    fn a_gateway_on_the_subnet_is_accepted() {
        let interface = plan(&net("10.0.2.15", 24, "10.0.2.2"));
        assert_eq!(interface.address, Ipv4Addr::new(10, 0, 2, 15));
        assert_eq!(interface.netmask, Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(interface.gateway, Ipv4Addr::new(10, 0, 2, 2));
    }
}
