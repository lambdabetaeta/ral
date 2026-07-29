//! The one child: the ral/exarch engine, and everything the daemon decides
//! on its behalf.
//!
//! The engine is the guest's entire reason for existing.  It is the same
//! multicall binary the host-side front-end spawns, run under `--engine`
//! (`core/src/engine.rs::run_engine`), and it expects exactly one thing from
//! whoever starts it: its protocol socket on file descriptor 3.  On the host
//! that socket is one end of a `socketpair`; in the guest it is a connection
//! to the host over `AF_VSOCK`.  The engine cannot tell the difference, and
//! should not have to.
//!
//! What the daemon decides here is small and worth stating: the command
//! line, the environment (a closed set — nothing of the daemon's own reaches
//! the engine), the working directory (the workspace, the only thing in the
//! guest worth being anchored to), and the descriptors.  All four are data,
//! and tested as data; [`spawn`] is the thin edge that performs them.
//!
//! ## Supervision: the engine is never restarted
//!
//! One VM per session, one engine per VM, and the whole session lives in the
//! engine's memory.  A restarted engine would come back having forgotten
//! every session id the host is still holding, and the host's next frame
//! would land on a stranger wearing its name.  The design record already
//! settles what happens instead — "a guest crash loses guest session state;
//! the host retains the last acknowledged checkpoint" — so the daemon does
//! the honest thing: it reaps the engine, writes its [`epitaph`] to the
//! console, and powers the machine off.  The host observes a VM that
//! stopped, which is unambiguous, rather than a VM that is still running and
//! quietly wrong.

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::process::Command;
use std::time::{Duration, Instant};

use rustix::process::Pid;

use crate::boot::Boot;
use crate::mounts::WORK;
use crate::reap::Death;
use crate::vsock;

/// The descriptor the engine reads its protocol from.  Fixed by
/// `run_engine`, which adopts fd 3 unconditionally.
pub const PROTOCOL_FD: RawFd = 3;

/// The environment every engine gets — and, being given it after an
/// `env_clear`, the whole of what an un-networked boot has.
///
/// Nothing here is inherited: the daemon's own environment is whatever the
/// kernel handed PID 1, and passing it on would make the guest's behaviour
/// depend on the boot artifact's incidental state.  There are no
/// credentials to leak — provider traffic never enters the guest — but the
/// closure is worth having anyway, because a closed environment is one
/// fewer thing that can differ between two boots of the same image.
const BASE: &[(&str, &str)] = &[
    ("HOME", "/root"),
    (
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    ),
    ("TMPDIR", "/tmp"),
    ("LANG", "C.UTF-8"),
    // The engine speaks a protocol, not a terminal; anything that probes
    // TERM should find an honest answer rather than a capability list it
    // cannot use.
    ("TERM", "dumb"),
    // The sole channel by which `core::engine::run_engine` (the same
    // multicall binary run here) learns it is running inside a guest, so
    // it can install the process jail onto the shell it boots.
    ("RAL_GUEST", "1"),
];

/// The gateway's port for the CONNECT-only proxy — `guest-net`'s one
/// listening endpoint.
const PROXY_PORT: u16 = 3128;

/// Hosts a networked boot's own server should never be routed through the
/// proxy to reach.
const NO_PROXY: &str = "localhost,127.0.0.1,::1";

/// The engine's environment for this boot: [`BASE`] always, plus — only when
/// `gateway` is `Some`, i.e. this boot has a network — both conventional
/// spellings of `HTTPS_PROXY` and [`NO_PROXY`].
///
/// `HTTP_PROXY`/`http_proxy` are deliberately not set: only HTTPS goes
/// through this door, so a plain `http://` URL dies as a connection reset
/// rather than a proxy error — answering it well would mean parsing
/// absolute-form requests, a second parse surface this door does not have.
pub fn environment(gateway: Option<Ipv4Addr>) -> Vec<(&'static str, String)> {
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

/// How long the daemon keeps trying to reach the host's control-plane
/// listener before giving up.  The host sets its listener up before it boots
/// the VM, so the first attempt normally wins; this covers the race, not an
/// absent host.
const PATIENCE: Duration = Duration::from_secs(5);

/// How long to wait between attempts at the control plane.
const RETRY: Duration = Duration::from_millis(100);

/// The engine's command line.
///
/// `--engine` is the flag both host binaries dispatch on before `main`, so
/// this is the whole of it: the engine takes its configuration from the
/// `Attach` frame the host sends over the socket, not from argv.
pub fn command_line(boot: &Boot) -> Vec<String> {
    vec![boot.engine.clone(), "--engine".to_string()]
}

/// Connect the guest's end of the control plane.
///
/// The guest dials the host — `VMADDR_CID_HOST`, the port the command line
/// named — rather than listening for it.  A connection that exists is then
/// proof the host is there, and the daemon needs no accept loop, no
/// readiness handshake of its own, and no opinion about what travels over
/// it.  The dial itself is [`vsock::dial_host`], shared with the workspace's
/// 9p transport; what belongs to the control plane alone is the patience
/// below.
///
/// # Errors
/// Returns a sentence naming the port when the host cannot be reached within
/// [`PATIENCE`].
pub fn control_plane(port: u32) -> Result<OwnedFd, String> {
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

/// Start the engine on `control`, and return its pid.
///
/// The child leads its own session, so the terminal signals a guest can
/// generate never reach PID 1, and one `kill(-pid, …)` at shutdown reaches
/// the engine and everything it spawned.  It inherits the daemon's standard
/// descriptors — which the kernel connected to `/dev/console` — so anything
/// the engine writes outside the protocol lands in the host's log beside the
/// daemon's own lines.
///
/// `control` stays borrowed: the parent's copy is closed by the caller once
/// the child holds its own.
///
/// # Errors
/// Returns a sentence naming the engine binary when it cannot be started.
pub fn spawn(boot: &Boot, control: &OwnedFd) -> Result<Pid, String> {
    let socket = control.as_raw_fd();
    let mut cmd = Command::new(&boot.engine);
    cmd.arg("--engine");
    cmd.env_clear();
    cmd.envs(environment(boot.net.as_ref().map(|net| net.gateway)));
    cmd.current_dir(WORK);
    // SAFETY: the closure runs between `fork` and `exec` and calls only
    // async-signal-safe syscalls — `setsid`, `dup2`, `close` — with no
    // allocation and no locking.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if socket != PROTOCOL_FD {
                if libc::dup2(socket, PROTOCOL_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
                libc::close(socket);
            }
            // The socket is created `SOCK_CLOEXEC` and usually already sits
            // on `PROTOCOL_FD`, where `dup2` is a no-op that leaves that flag
            // set — so clear it explicitly, unconditionally, rather than
            // leaning on `dup2`'s clear-on-copy. Without this the wire closes
            // on `exec` and the engine adopts a dead fd. The engine sets
            // `CLOEXEC` again the instant it owns the socket.
            if libc::fcntl(PROTOCOL_FD, libc::F_SETFD, 0) < 0 {
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

/// What the engine's death means, said plainly enough for a host-side log.
///
/// The policy in one sentence: there is no restart, because there is nothing
/// to restart *into* — the session the host is holding lived in the memory
/// this process just lost.
pub fn epitaph(death: Death) -> String {
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
            workspace: crate::boot::Export::Virtiofs { tag: "work".into() },
            port: 1729,
            epoch: 0,
            engine: "/usr/libexec/ral/engine".into(),
            net: None,
        }
    }

    /// The engine is the multicall binary under one flag; its configuration
    /// arrives over the socket, never on argv.
    #[test]
    fn the_command_line_is_the_binary_under_one_flag() {
        assert_eq!(
            command_line(&boot()),
            vec![
                "/usr/libexec/ral/engine".to_string(),
                "--engine".to_string()
            ]
        );
    }

    const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

    /// The environment is a set: one value per name, none of them empty —
    /// networked or not.
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

    /// `RAL_GUEST` is the entire signal `core::engine::run_engine` reads to
    /// know it is booting inside a guest, so it must always be set and
    /// non-empty, whether or not this boot has a network.
    #[test]
    fn ral_guest_is_set_and_non_empty() {
        for gateway in [None, Some(GATEWAY)] {
            let value = environment(gateway)
                .into_iter()
                .find_map(|(name, value)| (name == "RAL_GUEST").then_some(value))
                .expect("the engine is given RAL_GUEST");
            assert!(!value.is_empty());
        }
    }

    /// Every directory the engine will search for a command is absolute:
    /// the guest has no working directory a relative `PATH` entry could
    /// sensibly mean.
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

    /// An un-networked boot carries none of the proxy variables; a networked
    /// one carries all four, `HTTPS_PROXY` pointing at the gateway's
    /// CONNECT door.
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

    /// A clean exit is the session ending, not a failure, and the log says
    /// so; anything else names how the engine died.
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
