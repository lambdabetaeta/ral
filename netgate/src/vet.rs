//! Only vetted public addresses are dialled, after one resolution.

use std::borrow::Cow;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

use ral_core::egress::Host;

use crate::refusal::Refusal;
use crate::site::{Silent, site};

pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        // Classify both IPv4-mapped and IPv4-compatible addresses as IPv4.
        IpAddr::V6(v6) => match v6.to_ipv4() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    if o[0] == 0 {
        return false; // 0.0.0.0/8, unspecified
    }
    if v4.is_broadcast()
        || v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_documentation()
        || v4.is_multicast()
    {
        return false;
    }
    if o[0] == 100 && (64..=127).contains(&o[1]) {
        return false; // 100.64.0.0/10, carrier-grade NAT
    }
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        return false; // 198.18.0.0/15, benchmarking
    }
    if o[0] == 192 && o[1] == 0 && o[2] == 0 {
        return false; // 192.0.0.0/24, IETF protocol assignments
    }
    if o[0] >= 240 {
        return false; // 240.0.0.0/4, reserved
    }
    true
}

fn is_public_v6(v6: Ipv6Addr) -> bool {
    let s = v6.segments();
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return false; // 2001:db8::/32, documentation
    }
    if s[0] == 0x2002 {
        return false; // 2002::/16, 6to4: embeds an IPv4 address in bytes 2..6
    }
    if s[0] == 0x2001 && s[1] == 0x0000 {
        return false; // 2001::/32, Teredo: embeds an IPv4 address
    }
    if s[0] == 0x2001 && s[1] == 0x0002 && s[2] == 0x0000 {
        return false; // 2001:2::/48, benchmarking
    }
    if s[0] == 0x2001 && (s[1] & 0xfff0) == 0x0010 {
        return false; // 2001:10::/28, ORCHID
    }
    if s[0] == 0x2001 && (s[1] & 0xfff0) == 0x0020 {
        return false; // 2001:20::/28, ORCHIDv2
    }
    if s[0] == 0x3fff && (s[1] & 0xf000) == 0 {
        return false; // 3fff::/20, documentation (RFC 9637)
    }
    s[0] >> 13 == 0b001 // 2000::/3, global unicast
}

pub trait Dialer: Send + Sync {
    /// # Errors
    /// Reports resolver failure for the host's port-443 addresses.
    fn resolve(&self, host: &Host) -> io::Result<Vec<SocketAddr>>;

    /// # Errors
    /// Whatever the connect attempt reports.
    fn dial(&self, addr: SocketAddr, timeout: Duration) -> io::Result<TcpStream>;
}

pub struct System;

impl Dialer for System {
    fn resolve(&self, host: &Host) -> io::Result<Vec<SocketAddr>> {
        use std::net::ToSocketAddrs;
        Ok(site!(Silent::VetResolve, (host.as_str(), 443u16) => to_socket_addrs())?.collect())
    }

    fn dial(&self, addr: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
        site!(Silent::VetDial, TcpStream::connect_timeout(&addr, timeout))
    }
}

/// # Errors
/// Refuses resolution failure, no public addresses, or failure of every vetted dial.
pub fn open(
    host: &Host,
    dialer: &dyn Dialer,
    timeout: Duration,
) -> Result<(TcpStream, SocketAddr), Refusal> {
    let addrs = dialer
        .resolve(host)
        .ok()
        .filter(|addrs| !addrs.is_empty())
        .ok_or(Refusal {
            status: 502,
            reason: Cow::Borrowed("resolution failed"),
        })?;
    let public: Vec<SocketAddr> = addrs.into_iter().filter(|a| is_public(a.ip())).collect();
    if public.is_empty() {
        return Err(Refusal {
            status: 403,
            reason: Cow::Borrowed("no public address"),
        });
    }
    public
        .iter()
        .find_map(|&addr| dialer.dial(addr, timeout).ok().map(|stream| (stream, addr)))
        .ok_or(Refusal {
            status: 502,
            reason: Cow::Borrowed("no address answered"),
        })
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn unspecified_ipv4_is_rejected() {
        assert!(!is_public("0.0.0.0".parse().unwrap()));
        assert!(!is_public("0.255.255.255".parse().unwrap()));
    }

    #[test]
    fn broadcast_ipv4_is_rejected() {
        assert!(!is_public("255.255.255.255".parse().unwrap()));
    }

    #[test]
    fn loopback_ipv4_is_rejected() {
        assert!(!is_public("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn private_use_ipv4_is_rejected() {
        assert!(!is_public("10.1.2.3".parse().unwrap()));
        assert!(!is_public("172.16.0.1".parse().unwrap()));
        assert!(!is_public("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn link_local_ipv4_is_rejected() {
        assert!(!is_public("169.254.169.254".parse().unwrap()));
    }

    #[test]
    fn carrier_grade_nat_ipv4_is_rejected() {
        assert!(!is_public("100.64.0.1".parse().unwrap()));
        assert!(!is_public("100.127.255.255".parse().unwrap()));
    }

    #[test]
    fn documentation_ipv4_is_rejected() {
        assert!(!is_public("192.0.2.1".parse().unwrap()));
        assert!(!is_public("198.51.100.1".parse().unwrap()));
        assert!(!is_public("203.0.113.1".parse().unwrap()));
    }

    #[test]
    fn benchmarking_ipv4_is_rejected() {
        assert!(!is_public("198.18.0.1".parse().unwrap()));
        assert!(!is_public("198.19.255.255".parse().unwrap()));
    }

    #[test]
    fn ietf_protocol_assignments_ipv4_is_rejected() {
        assert!(!is_public("192.0.0.8".parse().unwrap()));
    }

    #[test]
    fn multicast_ipv4_is_rejected() {
        assert!(!is_public("224.0.0.1".parse().unwrap()));
    }

    #[test]
    fn reserved_ipv4_is_rejected() {
        assert!(!is_public("240.0.0.1".parse().unwrap()));
    }

    #[test]
    fn unspecified_ipv6_is_rejected() {
        assert!(!is_public("::".parse().unwrap()));
    }

    #[test]
    fn loopback_ipv6_is_rejected() {
        assert!(!is_public("::1".parse().unwrap()));
    }

    #[test]
    fn unique_local_ipv6_is_rejected() {
        assert!(!is_public("fc00::1".parse().unwrap()));
    }

    #[test]
    fn link_local_ipv6_is_rejected() {
        assert!(!is_public("fe80::1".parse().unwrap()));
    }

    #[test]
    fn documentation_ipv6_is_rejected() {
        assert!(!is_public("2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn multicast_ipv6_is_rejected() {
        assert!(!is_public("ff00::1".parse().unwrap()));
    }

    #[test]
    fn non_global_unicast_ipv6_is_rejected() {
        assert!(!is_public("4000::1".parse().unwrap()));
    }

    #[test]
    fn six_to_four_ipv6_is_rejected() {
        assert!(!is_public("2002::1".parse().unwrap()));
    }

    #[test]
    fn six_to_four_wrapped_link_local_ipv4_is_rejected() {
        assert!(!is_public("2002:a9fe:a9fe::".parse().unwrap()));
    }

    #[test]
    fn teredo_ipv6_is_rejected() {
        assert!(!is_public("2001::1".parse().unwrap()));
    }

    #[test]
    fn benchmarking_ipv6_is_rejected() {
        assert!(!is_public("2001:2::1".parse().unwrap()));
    }

    #[test]
    fn orchid_ipv6_is_rejected() {
        assert!(!is_public("2001:10::1".parse().unwrap()));
    }

    #[test]
    fn orchidv2_ipv6_is_rejected() {
        assert!(!is_public("2001:20::1".parse().unwrap()));
    }

    #[test]
    fn documentation_ipv6_rfc9637_is_rejected() {
        assert!(!is_public("3fff::1".parse().unwrap()));
    }

    #[test]
    fn public_ipv4_is_admitted() {
        assert!(is_public("93.184.216.34".parse().unwrap()));
    }

    #[test]
    fn public_ipv6_is_admitted() {
        assert!(is_public(
            "2606:2800:220:1:248:1893:25c8:1946".parse().unwrap()
        ));
    }

    #[test]
    fn ipv4_mapped_private_address_is_rejected() {
        assert!(!is_public("::ffff:10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn ipv4_mapped_link_local_address_is_rejected() {
        assert!(!is_public("::ffff:169.254.169.254".parse().unwrap()));
    }

    struct FakeDialer {
        resolve_calls: AtomicUsize,
        resolve_result: io::Result<Vec<SocketAddr>>,
        dial_calls: Mutex<Vec<SocketAddr>>,
    }

    impl FakeDialer {
        fn returning(addrs: Vec<SocketAddr>) -> Self {
            Self {
                resolve_calls: AtomicUsize::new(0),
                resolve_result: Ok(addrs),
                dial_calls: Mutex::new(Vec::new()),
            }
        }

        fn erroring() -> Self {
            Self {
                resolve_calls: AtomicUsize::new(0),
                resolve_result: Err(io::Error::other("resolution boom")),
                dial_calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Dialer for FakeDialer {
        fn resolve(&self, _host: &Host) -> io::Result<Vec<SocketAddr>> {
            self.resolve_calls.fetch_add(1, Ordering::SeqCst);
            match &self.resolve_result {
                Ok(addrs) => Ok(addrs.clone()),
                Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
            }
        }

        fn dial(&self, addr: SocketAddr, _timeout: Duration) -> io::Result<TcpStream> {
            self.dial_calls.lock().unwrap().push(addr);
            Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "no fixture listener",
            ))
        }
    }

    #[test]
    fn mixed_public_and_private_answer_keeps_only_public_addresses_in_order() {
        let addrs = vec![
            "10.0.0.1:443".parse().unwrap(),
            "93.184.216.34:443".parse().unwrap(),
            "192.168.1.1:443".parse().unwrap(),
        ];
        let dialer = FakeDialer::returning(addrs);
        let host = Host::parse("example.com").unwrap();
        let _ = open(&host, &dialer, Duration::from_millis(50));
        assert_eq!(
            *dialer.dial_calls.lock().unwrap(),
            vec!["93.184.216.34:443".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn all_private_answer_refuses_with_403_and_never_dials() {
        let addrs = vec![
            "10.0.0.1:443".parse().unwrap(),
            "192.168.1.1:443".parse().unwrap(),
        ];
        let dialer = FakeDialer::returning(addrs);
        let host = Host::parse("example.com").unwrap();
        let refusal = open(&host, &dialer, Duration::from_millis(50)).unwrap_err();
        assert_eq!(
            refusal,
            Refusal {
                status: 403,
                reason: Cow::Borrowed("no public address")
            }
        );
        assert!(dialer.dial_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn resolver_error_refuses_and_never_dials() {
        let dialer = FakeDialer::erroring();
        let host = Host::parse("example.com").unwrap();
        let refusal = open(&host, &dialer, Duration::from_millis(50)).unwrap_err();
        assert_eq!(
            refusal,
            Refusal {
                status: 502,
                reason: Cow::Borrowed("resolution failed")
            }
        );
        assert!(dialer.dial_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn dialer_receives_the_exact_vetted_socket_addr() {
        let addrs = vec!["93.184.216.34:443".parse().unwrap()];
        let dialer = FakeDialer::returning(addrs);
        let host = Host::parse("example.com").unwrap();
        let _ = open(&host, &dialer, Duration::from_millis(50));
        assert_eq!(
            *dialer.dial_calls.lock().unwrap(),
            vec!["93.184.216.34:443".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn second_address_is_tried_after_the_first_fails_and_resolve_runs_once() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let real_addr = listener.local_addr().unwrap();
        // Vet the public-looking answers, but connect only to the loopback fixture.
        let addrs = vec![
            "93.184.216.1:443".parse().unwrap(),
            "93.184.216.2:443".parse().unwrap(),
        ];
        let dialer = FakeDialer::returning(addrs);
        let dialer = FakeDialerWithSelectiveFailure {
            inner: dialer,
            real_addr,
            fail_first: "93.184.216.1:443".parse().unwrap(),
        };
        let host = Host::parse("example.com").unwrap();
        let (_, answered) = open(&host, &dialer, Duration::from_millis(200)).unwrap();
        assert_eq!(answered, "93.184.216.2:443".parse::<SocketAddr>().unwrap());
        assert_eq!(dialer.inner.resolve_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *dialer.inner.dial_calls.lock().unwrap(),
            vec![
                "93.184.216.1:443".parse::<SocketAddr>().unwrap(),
                "93.184.216.2:443".parse::<SocketAddr>().unwrap()
            ]
        );
    }

    struct FakeDialerWithSelectiveFailure {
        inner: FakeDialer,
        real_addr: SocketAddr,
        fail_first: SocketAddr,
    }

    impl Dialer for FakeDialerWithSelectiveFailure {
        fn resolve(&self, host: &Host) -> io::Result<Vec<SocketAddr>> {
            self.inner.resolve(host)
        }

        fn dial(&self, addr: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
            self.inner.dial_calls.lock().unwrap().push(addr);
            if addr == self.fail_first {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "first address refuses",
                ));
            }
            TcpStream::connect_timeout(&self.real_addr, timeout)
        }
    }
}
