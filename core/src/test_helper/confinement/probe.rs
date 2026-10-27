//! `RAL_TEST_PROBE` holds a JSON argument array; warrants take precedence.

use std::io::{self, Write};

/// Encode the role without depending on constructor-time argv.
///
/// # Panics
/// If JSON serialization fails.
pub fn request(args: &[&str]) -> String {
    serde_json::to_string(args).expect("probe arguments encode")
}

/// Serve a probe before libtest starts.
///
/// # Panics
/// If its output cannot be written.
pub fn serve() -> Option<u8> {
    let request = std::env::var_os("RAL_TEST_PROBE")?;
    let result = request
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "RAL_TEST_PROBE must be UTF-8"))
        .and_then(|request| serde_json::from_str::<Vec<String>>(request).map_err(io::Error::other))
        .and_then(|args| run(&args));
    Some(match result {
        Ok(()) => {
            writeln!(io::stdout(), "ok").expect("write probe marker");
            0
        }
        Err(error) => {
            writeln!(io::stderr(), "{error}").expect("write probe error");
            1
        }
    })
}

fn run(args: &[String]) -> io::Result<()> {
    match args {
        #[cfg(unix)]
        [cmd, fds @ ..] if cmd == "fds" => {
            for fd in fds {
                let fd: i32 = fd.parse().map_err(io::Error::other)?;
                if fd < 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "a descriptor is nonnegative",
                    ));
                }
                // SAFETY: F_GETFD inspects the descriptor without borrowing it.
                if unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0 {
                    writeln!(io::stdout(), "{fd} open")?;
                } else if io::Error::last_os_error().raw_os_error() != Some(libc::EBADF) {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        }
        #[cfg(unix)]
        [cmd] if cmd == "pty" => {
            use std::os::fd::{FromRawFd, OwnedFd};
            let (mut master, mut slave) = (-1, -1);
            // SAFETY: both outputs are writable; optional settings are absent.
            if unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: openpty returned two fresh descriptors.
            let (_master, _slave) =
                unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
            Ok(())
        }
        #[cfg(unix)]
        [cmd, path] if cmd == "pty" => std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map(drop),
        #[cfg(unix)]
        [cmd, path] if cmd == "unix" => std::os::unix::net::UnixStream::connect(path).map(drop),
        #[cfg(target_os = "linux")]
        [cmd, name] if cmd == "abstract" => {
            use std::os::linux::net::SocketAddrExt;
            let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
            std::os::unix::net::UnixStream::connect_addr(&addr).map(drop)
        }
        #[cfg(target_os = "linux")]
        [cmd, name] if cmd == "abstract-pair" => {
            use std::io::Read;
            use std::os::linux::net::SocketAddrExt;
            use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};

            let addr = SocketAddr::from_abstract_name(name)?;
            let listener = UnixListener::bind_addr(&addr)?;
            let mut connector = UnixStream::connect_addr(&addr)?;
            let (mut peer, _) = listener.accept()?;
            for socket in [&connector, &peer] {
                socket.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
                socket.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
            }
            connector.write_all(b"ping")?;
            let mut message = [0; 4];
            peer.read_exact(&mut message)?;
            if &message != b"ping" {
                return Err(io::Error::other("abstract listener did not receive ping"));
            }
            peer.write_all(b"pong")?;
            connector.read_exact(&mut message)?;
            if &message != b"pong" {
                return Err(io::Error::other("abstract connector did not receive pong"));
            }
            Ok(())
        }
        [cmd, addr] if cmd == "tcp" => std::net::TcpStream::connect(addr).map(drop),
        [cmd, addr] if cmd == "udp" => {
            let addr: std::net::SocketAddr = addr.parse().map_err(io::Error::other)?;
            let socket = std::net::UdpSocket::bind(if addr.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            })?;
            socket.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
            socket.connect(addr)?;
            socket.send(b"ok")?;
            let mut reply = [0; 3];
            let length = socket.recv(&mut reply)?;
            if &reply[..length] != b"ok" {
                return Err(io::Error::other("UDP peer did not echo the probe datagram"));
            }
            Ok(())
        }
        [cmd, name] if cmd == "resolve" => {
            use std::net::ToSocketAddrs;
            (name.as_str(), 0)
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "resolver returned no addresses")
                })
                .map(drop)
        }
        #[cfg(target_os = "macos")]
        [cmd, name] if cmd == "shm" => {
            use std::os::fd::{FromRawFd, OwnedFd};
            let name = std::ffi::CString::new(name.as_bytes()).map_err(io::Error::other)?;
            // SAFETY: name is NUL-terminated; this opens an existing object.
            let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
            if fd < 0 {
                Err(io::Error::last_os_error())
            } else {
                // SAFETY: shm_open returned a fresh descriptor.
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
                Ok(())
            }
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown probe arguments: {args:?}"),
        )),
    }
}
