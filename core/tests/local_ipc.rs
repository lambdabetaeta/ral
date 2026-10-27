#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(all(feature = "test-util", any(target_os = "linux", target_os = "macos")))]

mod common;

use ral_core::authority::policy::{Capabilities, FsPolicy, Net};
use ral_core::path::FrozenPath;
use ral_core::test_helper::confinement::{Output, admitted, denied, probe, run};
use ral_core::test_helper::{layer, stack};
use std::net::{TcpListener, UdpSocket};
use std::path::Path;
use std::time::Duration;

const OK: &str = "ok\n";

fn confined(root: &Path, offline: bool, args: &[&str], env: &[(&str, &str)]) -> Output {
    let exe = std::env::current_exe().unwrap();
    let caps = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![
                FrozenPath::freeze(root).unwrap(),
                FrozenPath::freeze(exe.parent().unwrap()).unwrap(),
            ],
            ..FsPolicy::default()
        }),
        net: offline.then_some(Net::Offline),
        ..Capabilities::default()
    };
    let request = probe::request(args);
    let env = std::iter::once(("RAL_TEST_PROBE", request.as_str()))
        .chain(env.iter().copied())
        .map(|(key, value)| format!("{key}: '{value}'"))
        .collect::<Vec<_>>()
        .join(", ");
    run(
        stack([layer(caps)]),
        &format!("within [env: [{env}]] {{ {} }}", exe.display()),
    )
}

fn ambient(args: &[&str]) -> Output {
    std::process::Command::new(std::env::current_exe().unwrap())
        .env("RAL_TEST_PROBE", probe::request(args))
        .output()
        .expect("the ambient probe starts")
        .into()
}

fn echo_datagram(peer: &UdpSocket) -> std::thread::JoinHandle<()> {
    let peer = peer.try_clone().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    std::thread::spawn(move || {
        let mut message = [0; 3];
        let (length, sender) = peer.recv_from(&mut message).unwrap();
        assert_eq!(&message[..length], b"ok");
        assert_eq!(peer.send_to(&message[..length], sender).unwrap(), length);
    })
}

fn assert_no_datagram(peer: &UdpSocket) {
    peer.set_nonblocking(true).unwrap();
    assert_eq!(
        peer.recv_from(&mut [0; 3]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "the host peer must receive exactly the online probe's one datagram"
    );
}

#[test]
fn offline_tcp_and_udp_cannot_reach_host_peers() {
    let proof = ral_core::require_confinement!();
    let root = tempfile::tempdir().unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    let echo = echo_datagram(&udp);
    let control = confined(root.path(), true, &["fds", "0", "1", "2"], &[]);
    admitted(&proof, &control, OK);
    for (kind, address) in [
        ("tcp", tcp.local_addr().unwrap()),
        ("udp", udp.local_addr().unwrap()),
    ] {
        let address = address.to_string();
        let args = [kind, address.as_str()];
        admitted(&proof, &confined(root.path(), false, &args, &[]), OK);
        let out = confined(root.path(), true, &args, &[]);
        denied(&proof, &control, &out, OK);
        assert_eq!(out.status, 1, "{out:?}");
        assert!(!out.stderr.is_empty(), "{out:?}");
    }
    echo.join().unwrap();
    assert_no_datagram(&udp);
}

#[cfg(target_os = "linux")]
#[test]
fn abstract_sockets_reach_only_peers_inside_the_confined_launch() {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};

    let proof = ral_core::require_confinement!();
    ral_core::require_landlock!(6);
    let root = tempfile::tempdir().unwrap();
    let name = format!("ral-ipc-{}-{:032x}", std::process::id(), fastrand::u128(..));
    let address = SocketAddr::from_abstract_name(&name).unwrap();
    let _listener = UnixListener::bind_addr(&address).unwrap();
    admitted(&proof, &ambient(&["abstract", &name]), OK);
    let control = confined(
        root.path(),
        false,
        &["abstract-pair", &format!("{name}-inside")],
        &[],
    );
    let out = confined(root.path(), false, &["abstract", &name], &[]);
    denied(&proof, &control, &out, OK);
    assert_eq!(out.status, 1, "{out:?}");
    assert!(!out.stderr.is_empty(), "{out:?}");
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::net::UnixListener;

    #[test]
    fn online_allows_ip_and_resolution_but_never_the_readable_unix_socket() {
        let proof = ral_core::require_confinement!();
        let resolver = "/private/var/run/mDNSResponder";
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("agent.sock");
        let path = path.to_str().unwrap();
        let _unix = UnixListener::bind(path).unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tcp = tcp.local_addr().unwrap().to_string();
        let udp_address = udp.local_addr().unwrap().to_string();
        admitted(&proof, &ambient(&["unix", path]), OK);

        for env in [&[][..], &[("SSH_AUTH_SOCK", path)][..]] {
            let control = confined(root.path(), false, &["tcp", &tcp], env);
            denied(
                &proof,
                &control,
                &confined(root.path(), false, &["unix", path], env),
                OK,
            );
        }
        let echo = echo_datagram(&udp);
        admitted(
            &proof,
            &confined(root.path(), false, &["udp", &udp_address], &[]),
            OK,
        );
        echo.join().unwrap();
        admitted(
            &proof,
            &confined(root.path(), false, &["unix", resolver], &[]),
            OK,
        );
        admitted(
            &proof,
            &confined(root.path(), false, &["resolve", "localhost"], &[]),
            OK,
        );

        let control = confined(root.path(), true, &["fds", "0", "1", "2"], &[]);
        for args in [
            ["unix", path],
            ["tcp", &tcp],
            ["udp", &udp_address],
            ["unix", resolver],
        ] {
            let out = confined(root.path(), true, &args, &[]);
            assert_eq!(out.status, 1, "{args:?}: {out:?}");
            denied(&proof, &control, &out, OK);
            assert!(!out.stderr.is_empty(), "{args:?}: {out:?}");
        }
        assert_no_datagram(&udp);
    }

    struct SharedMemory {
        name: CString,
        _fd: OwnedFd,
    }

    impl SharedMemory {
        fn new() -> Self {
            let name = CString::new(format!("/ral-{:016x}", fastrand::u64(..))).unwrap();
            // SAFETY: name is NUL-terminated; O_EXCL creates only our object.
            let fd = unsafe {
                libc::shm_open(
                    name.as_ptr(),
                    libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                    0o600,
                )
            };
            assert!(
                fd >= 0,
                "create host shm: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: shm_open returned a fresh descriptor.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            Self { name, _fd: fd }
        }
    }

    impl Drop for SharedMemory {
        fn drop(&mut self) {
            // SAFETY: this name belongs to the fixture's O_EXCL creation.
            assert_eq!(unsafe { libc::shm_unlink(self.name.as_ptr()) }, 0);
        }
    }

    #[test]
    fn host_shared_memory_is_denied_while_a_confined_echo_starts() {
        let proof = ral_core::require_confinement!();
        let root = tempfile::tempdir().unwrap();
        let shm = SharedMemory::new();
        let args = ["shm", shm.name.to_str().unwrap()];
        admitted(&proof, &ambient(&args), OK);
        let control = confined(root.path(), false, &["fds", "0", "1", "2"], &[]);
        let out = confined(root.path(), false, &args, &[]);
        denied(&proof, &control, &out, OK);
        assert_eq!(out.status, 1, "{out:?}");
        let caps = Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec![FrozenPath::freeze(root.path()).unwrap()],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        };
        admitted(&proof, &run(stack([layer(caps)]), "/bin/echo ok"), OK);
    }
}
