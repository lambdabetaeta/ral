//! Engines receive a closed command/environment and an unread protocol stream on fd 3.
//! The root is never restarted: its lost memory held the host's session state.

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::Command;
use std::time::{Duration, Instant};

use rustix::process::Pid;

use crate::reap::{self, Death};
use crate::vsock;
use guest_abi::boot::Boot;
use guest_abi::layout::{ENGINE_FD, ENGINE_FLAG, EXEC_CGROUP, SCRATCH, WORK};
use guest_abi::link::PROXY_PORT;

/// The entire unnetworked environment; nothing is inherited from PID 1.
const BASE: &[(&str, &str)] = &[
    ("HOME", "/root"),
    (
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    ),
    ("TMPDIR", SCRATCH),
    ("LANG", "C.UTF-8"),
    // TERM=dumb: the engine speaks a protocol and has no terminal capabilities.
    ("TERM", "dumb"),
];

const NO_PROXY: &str = "localhost,127.0.0.1,::1";

/// `HTTP_PROXY` stays unset: absolute-form HTTP requests would require a second parser.
pub(crate) fn environment(gateway: Option<Ipv4Addr>) -> Vec<(&'static str, String)> {
    let mut env: Vec<(&'static str, String)> = BASE
        .iter()
        .map(|(name, value)| (*name, (*value).to_string()))
        .collect();
    if let Some(gateway) = gateway {
        let proxy = format!("http://{gateway}:{PROXY_PORT}");
        for name in ["HTTPS_PROXY", "https_proxy"] {
            env.push((name, proxy.clone()));
        }
        for name in ["NO_PROXY", "no_proxy"] {
            env.push((name, NO_PROXY.to_string()));
        }
    }
    env
}

/// The host listens before boot; retries cover the listener race.
const PATIENCE: Duration = Duration::from_secs(5);

const RETRY: Duration = Duration::from_millis(100);

fn command(boot: &Boot) -> Command {
    let mut cmd = Command::new(&boot.engine);
    cmd.args([ENGINE_FLAG, EXEC_CGROUP]);
    cmd.env_clear();
    cmd.envs(environment(boot.net.as_ref().map(|net| net.link().gateway)));
    cmd.current_dir(WORK);
    cmd
}

pub(crate) fn control_plane(port: u32) -> Result<OwnedFd, String> {
    let deadline = Instant::now() + PATIENCE;
    loop {
        match vsock::dial_host(port) {
            Ok(socket) => return Ok(socket),
            Err(err) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "could not reach the host's control plane on vsock port {port} after \
                         {}s: {err}. The host must be listening on that port before the guest \
                         boots — is `ral.port` the port the session manager published?",
                        PATIENCE.as_secs()
                    ));
                }
                std::thread::sleep(RETRY);
            }
        }
    }
}

/// Each engine leads its own session; kill(-pid) reaches its descendants without signaling PID 1.
pub(crate) fn spawn(boot: &Boot, connection: &OwnedFd) -> Result<Pid, String> {
    let socket = connection.as_raw_fd();
    let mut cmd = command(boot);
    // SAFETY: between fork and exec, only async-signal-safe syscalls; no allocation or locking.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            // Children inherit the awaited-signal block and must unmask before exec.
            reap::unblock()?;
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if socket != ENGINE_FD {
                if libc::dup2(socket, ENGINE_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
                libc::close(socket);
            }
            // Clear CLOEXEC even when the socket already equals ENGINE_FD and dup2 would do nothing.
            if libc::fcntl(ENGINE_FD, libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|err| {
        format!(
            "could not start the engine at {}: {err}. Is `ral.engine` the path the boot artifact \
             put the ral/exarch binary at?",
            boot.engine
        )
    })?;
    Ok(Pid::from_child(&child))
}

pub(crate) fn epitaph(death: Death) -> String {
    match death {
        Death::Exited(0) => {
            "the engine exited cleanly; the session is over, powering the machine off".to_string()
        }
        death => format!(
            "the engine {death}; the session cannot be resumed in place, powering the machine off"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot() -> Boot {
        Boot {
            workspace: guest_abi::boot::Export::Virtiofs { tag: "work".into() },
            port: 1729,
            epoch: 0,
            engine: "/usr/libexec/ral/engine".into(),
            net: None,
        }
    }

    #[test]
    fn the_command_names_the_guest_role_and_cgroup_root() {
        let boot = boot();
        let cmd = command(&boot);
        assert_eq!(cmd.get_program(), boot.engine.as_str());
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            ["--guest-engine", EXEC_CGROUP]
        );
        assert_eq!(cmd.get_current_dir(), Some(std::path::Path::new(WORK)));
    }

    const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

    #[test]
    fn the_environment_names_each_variable_once() {
        for gateway in [None, Some(GATEWAY)] {
            let env = environment(gateway);
            let mut names: Vec<_> = env.iter().map(|(name, _)| *name).collect();
            names.sort_unstable();
            let given = names.len();
            names.dedup();
            assert_eq!(names.len(), given, "a variable is set twice ({gateway:?})");
            assert!(
                env.iter().all(|(_, value)| !value.is_empty()),
                "an empty value is not a setting ({gateway:?})"
            );
        }
    }

    #[test]
    fn every_path_entry_is_absolute() {
        let path = environment(None)
            .into_iter()
            .find_map(|(name, value)| (name == "PATH").then_some(value))
            .expect("the engine is given a PATH");
        assert!(
            path.split(':').all(|dir| dir.starts_with('/')),
            "PATH carries a relative entry: {path}"
        );
    }

    #[test]
    fn proxy_variables_are_conditional_on_networked() {
        let names = ["HTTPS_PROXY", "https_proxy", "NO_PROXY", "no_proxy"];
        let unnetworked = environment(None);
        assert!(
            names
                .iter()
                .all(|name| unnetworked.iter().all(|(n, _)| n != name)),
            "an un-networked boot has no proxy to point at"
        );

        let networked = environment(Some(GATEWAY));
        for name in names {
            assert!(
                networked.iter().any(|(n, _)| *n == name),
                "a networked boot is missing {name}"
            );
        }
        assert!(
            networked
                .iter()
                .any(|(n, v)| *n == "HTTPS_PROXY" && v == "http://10.0.2.2:3128"),
            "HTTPS_PROXY does not name the gateway's proxy door"
        );
        assert!(
            networked
                .iter()
                .all(|(n, _)| *n != "HTTP_PROXY" && *n != "http_proxy"),
            "HTTP_PROXY is deliberately never set"
        );
    }

    #[test]
    fn the_epitaph_distinguishes_an_ending_from_a_death() {
        let ended = epitaph(Death::Exited(0));
        assert!(ended.contains("exited cleanly"), "{ended}");
        assert!(!ended.contains("cannot be resumed"), "{ended}");

        let crashed = epitaph(Death::Signalled(11));
        assert!(crashed.contains("killed by signal 11"), "{crashed}");
        assert!(crashed.contains("cannot be resumed"), "{crashed}");
    }
}
