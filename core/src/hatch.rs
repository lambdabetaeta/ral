//! The guest hatch: a parent engine spawning a child engine onto a vsock
//! connection to the host, and that child engine hydrating the seed it was
//! spawned with.
//!
//! The connection is opened from the host's side. The parent binds a port for
//! the duration of one spawn ([`listen_for_hatch`]) and tells the host where;
//! the thread over that listener accepts, checks the eight token bytes the
//! host writes, and hands the connection to [`hatch_over`], which mirrors
//! [`crate::transport::WireTransport::new`]'s re-exec shape with
//! `ral-daemon`'s own fd discipline: seed a fresh socketpair with one framed
//! [`EngineSeed`], spawn `current_exe --engine` with the connection on fd 3
//! and the seed's fd named by `RAL_ENGINE_SEED_FD`, and write
//! [`crate::transport::HATCH_ACK`] back — after the spawn, so a host that
//! hears the ack has a live child on the other end.
//!
//! [`apply_seed`] is the other end: called from `engine_session` once a
//! freshly booted shell exists, it hydrates the seed's scope and context,
//! then narrows the shell's capabilities to the seed's validated grant
//! through whichever host installed [`set_grant_narrower`] — core itself
//! carries no capability vocabulary of its own to resolve a grant tag
//! against.
//!
//! The sockets alone — [`crate::vsock`] — are Linux-only, since `AF_VSOCK`
//! only means something inside a real guest. Everything else here is plain
//! Unix process/socket plumbing, exercised in tests over a `UnixListener` and
//! `UnixStream` pairs standing in for the guest's port, the same vehicle the
//! wire seat's own tests already re-exec `--engine` over.
//!
//! Everything that exists only to serve the Linux-only calls or this file's
//! own tests is therefore unreachable in a plain non-Linux, non-test build —
//! an accurate fact about this milestone's shape, not a bug, so it is stated
//! once here rather than peppering every such item with its own
//! `#[allow(dead_code)]`.
#![cfg_attr(not(any(target_os = "linux", test)), allow(dead_code, unused_imports))]

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::child_eval::{EngineSeed, pack_seed};
use crate::process::ChildHandle;
use crate::serial::WireDecoder;
use crate::subprocess::install_shell_mobile;
use crate::types::{Capabilities, Shell};
use crate::wire::WireChannel;

/// The engine's protocol socket lands on this descriptor, exactly as
/// `run_engine` expects (`core/src/engine.rs`) and `ral-daemon` already
/// spawns onto (`PROTOCOL_FD`).
const PROTOCOL_FD: RawFd = 3;

/// The env var carrying the seed socketpair's fd number — the pipeline
/// helpers' env-named-fd pattern (`runtime/pipeline/helper.rs`), not a second
/// magic number.
pub(crate) const RAL_ENGINE_SEED_FD_ENV: &str = "RAL_ENGINE_SEED_FD";

/// `own`, the grant tag, the cwd — in, a narrowed [`Capabilities`] or a
/// refusal out. What `set_grant_narrower` registers.
type GrantNarrower = fn(&Capabilities, &str, &str) -> Result<Capabilities, String>;

/// Host-supplied grant-narrowing hook: `own ⊓ resolve_base(grant, cwd)`,
/// evaluated where the installer's own capability vocabulary lives — core
/// carries no base-tag lexicon of its own. Registered once, the same
/// `OnceLock` shape as [`crate::sandbox::set_child_shell_extension`].
static GRANT_NARROWER: OnceLock<GrantNarrower> = OnceLock::new();

/// Register the host's grant-narrowing policy — `exarch::policy::narrow` for
/// exarch's own installer. Must be called before a hatched seed can ever be
/// applied; subsequent calls are silently ignored.
pub fn set_grant_narrower(narrow: GrantNarrower) {
    let _ = GRANT_NARROWER.set(narrow);
}

/// One hatched child still being watched: the seed channel's kept end reads
/// EOF exactly when the child engine exits, since only the child's own copy
/// of the other end keeps it open.
struct Hatched {
    child: ChildHandle,
    seed: WireChannel,
}

/// The process-global hatch table. No thread, no signal handler: a hatch
/// sweeps it on entry, and `teardown` sweeps it once more as the engine
/// exits, so a zombie between sweeps is inert and bounded by the table.
static HATCHED: OnceLock<Mutex<Vec<Hatched>>> = OnceLock::new();

fn table() -> &'static Mutex<Vec<Hatched>> {
    HATCHED.get_or_init(|| Mutex::new(Vec::new()))
}

/// Reap every hatched child whose kept seed end has gone EOF; leave the rest.
fn sweep_hatched() {
    let mut table = table()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    table.retain_mut(
        |entry| match entry.seed.poll_readable(Some(Duration::ZERO)) {
            Ok(true) => {
                let _ = entry.child.wait_handling_stop(None, false);
                false
            }
            _ => true,
        },
    );
}

/// Called once, from `engine_session`'s teardown. Anything still running is
/// not killed: this engine's own exit reparents it to the daemon, whose reap
/// loop already has an orphan branch for exactly that arrival.
pub(crate) fn teardown_hatched() {
    sweep_hatched();
}

/// Why a listener thread has no child to report.
#[derive(Debug)]
pub enum Unhatched {
    /// Woken by its own caller before the host ever dialled, so it has no
    /// account of its own: whatever refused the spawn said so elsewhere, and
    /// that is the sentence to raise.
    Cancelled,
    /// Something this thread attempted failed, and this says what.
    Failed(String),
}

/// One spawn's listener: the thread that owns it, and the pipe that wakes it.
pub struct HatchListener {
    wake: io::PipeWriter,
    thread: JoinHandle<Result<u32, Unhatched>>,
}

impl HatchListener {
    /// Wake the thread out of whichever poll it is parked in, so a caller
    /// whose enquiry was refused can always have its thread back.
    pub fn cancel(&self) {
        let _ = (&self.wake).write(&[0]);
    }

    /// Wait for the thread, and answer the hatched child's pid.
    ///
    /// # Errors
    /// [`Unhatched::Failed`] if the thread got as far as a reason of its own;
    /// [`Unhatched::Cancelled`] if [`Self::cancel`] reached it first.
    pub fn join(self) -> Result<u32, Unhatched> {
        self.thread.join().unwrap_or_else(|_| {
            Err(Unhatched::Failed(
                "hatch: the thread listening for the host's dial panicked".to_string(),
            ))
        })
    }
}

/// Listen on an ephemeral guest port for the host to dial, and answer with
/// that port and the thread now waiting on it.
///
/// `token` is the eight bytes the host must write first. `shell` is packed
/// here, on the caller's own thread: the seed is all the listener thread ever
/// holds of the parent's session, and a `Shell` never leaves this one.
///
/// # Errors
/// Returns a sentence if the shell will not pack, or no port could be bound.
#[cfg(target_os = "linux")]
pub fn listen_for_hatch(
    token: u64,
    shell: &Shell,
    grant: String,
) -> Result<(u32, HatchListener), String> {
    let seed = packed_seed(shell, grant)?;
    let (listener, port) = crate::vsock::listen_any().map_err(|e| {
        format!(
            "hatch: could not bind a guest port for the host to dial: {e} — is this engine running \
             inside a VM with a vsock device?"
        )
    })?;
    Ok((port, hatch_listener(listener, token, seed)?))
}

/// The portable core of [`listen_for_hatch`]: `listener` is a bound,
/// listening socket — the guest's `AF_VSOCK` one, or a `UnixListener`'s in
/// tests.
fn hatch_listener(
    listener: OwnedFd,
    token: u64,
    seed: EngineSeed,
) -> Result<HatchListener, String> {
    let (woken, wake) = io::pipe()
        .map_err(|e| format!("hatch: could not open the pipe that wakes the listener: {e}"))?;
    let thread = std::thread::spawn(move || await_dial(&listener, &woken, token, &seed));
    Ok(HatchListener { wake, thread })
}

/// The listener thread's whole life: accept until the connection the host
/// dialled arrives, hatch onto it, and answer the child's pid.
///
/// Two poll sites and no clock. Neither blocks without the wake end in its
/// set, which is what makes the caller's join safe: the only party that can
/// leave this thread waiting is the host, whose own wait on the ack carries
/// the transport's deadline.
fn await_dial(
    listener: &OwnedFd,
    woken: &io::PipeReader,
    token: u64,
    seed: &EngineSeed,
) -> Result<u32, Unhatched> {
    loop {
        wait_readable(listener, woken)?;
        let connection = accept(listener)
            .map_err(|e| Unhatched::Failed(format!("hatch: could not accept a dial: {e}")))?;
        wait_readable(&connection, woken)?;

        let mut claim = [0u8; 8];
        let mut stream = UnixStream::from(connection);
        if stream.read_exact(&mut claim).is_err() || u64::from_le_bytes(claim) != token {
            // Whoever this was did not know the token, so it is not the host.
            // Losing the race costs it nothing but this connection.
            continue;
        }
        return hatch_over(stream.into(), seed).map_err(Unhatched::Failed);
    }
}

/// Block until `fd` is readable, or the caller writes the wake pipe.
fn wait_readable(fd: &impl AsFd, woken: &io::PipeReader) -> Result<(), Unhatched> {
    loop {
        let mut fds = [
            libc::pollfd {
                fd: fd.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: woken.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is two fully initialised pollfds and `2` is their
        // count; `poll` writes only `revents`. No timeout: this thread's only
        // clock is its caller.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Unhatched::Failed(format!(
                "hatch: could not wait for the host's dial: {err}"
            )));
        }
        // The wake end is read first: a caller that has already given up
        // outranks anything that arrived beside it.
        if fds[1].revents != 0 {
            return Err(Unhatched::Cancelled);
        }
        if fds[0].revents != 0 {
            return Ok(());
        }
    }
}

/// `accept(2)` with the peer's address discarded: a vsock peer is a CID this
/// guest has no use for, and std's `UnixListener` would refuse to read one as
/// a path.
fn accept(listener: &OwnedFd) -> io::Result<OwnedFd> {
    loop {
        // SAFETY: a plain `accept(2)`; a null address pair asks for no peer
        // name, and the descriptor is adopted immediately below.
        let raw = unsafe {
            libc::accept(
                listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if raw < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        // SAFETY: `raw` is a fresh descriptor this call owns.
        let connection = unsafe { OwnedFd::from_raw_fd(raw) };
        // `accept(2)` does not inherit the listener's CLOEXEC, and this fd
        // must not leak into every command the engine runs meanwhile;
        // `hatch_over` clears it on the one child that should have it.
        // SAFETY: a plain `fcntl(2)` on a descriptor this call owns.
        if unsafe { libc::fcntl(connection.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(connection);
    }
}

/// The forked session as the wire carries it: everything a child engine is
/// given, and all the listener thread ever holds.
fn packed_seed(shell: &Shell, grant: String) -> Result<EngineSeed, String> {
    use crate::types::Break;

    pack_seed(shell, grant).map_err(|b| match b {
        Break::Error(e) => format!("hatch: could not serialise the forked shell: {}", e.message),
        Break::Escape(_) => {
            "hatch: could not serialise the forked shell: unexpected escape".to_string()
        }
    })
}

/// The portable core of a hatch: everything past the connection itself,
/// which a `UnixStream` pair stands in for in tests.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:hatch-spawn] re-execs the current engine binary as a hatched child over a fresh vsock connection — infrastructure handoff exactly like WireTransport::new's engine-spawn door, not model turn-time I/O"
)]
fn hatch_over(connection: OwnedFd, seed: &EngineSeed) -> Result<u32, String> {
    sweep_hatched();

    let mut vsock = UnixStream::from(connection);
    let (mut parent_seed, child_seed) =
        UnixStream::pair().map_err(|e| format!("hatch: failed to open the seed channel: {e}"))?;
    crate::subprocess_codec::write_frame(&mut parent_seed, seed)
        .map_err(|e| format!("hatch: failed to write the engine seed: {e}"))?;

    let current_exe = std::env::current_exe()
        .map_err(|e| format!("hatch: could not resolve this engine's own executable path: {e}"))?;
    let mut cmd = Command::new(current_exe);
    cmd.arg("--engine");
    cmd.env(RAL_ENGINE_SEED_FD_ENV, child_seed.as_raw_fd().to_string());
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    let vsock_fd = vsock.as_raw_fd();
    let seed_fd = child_seed.as_raw_fd();
    // SAFETY: the closure runs between `fork` and `exec` and calls only
    // async-signal-safe syscalls — `dup2`, `close`, `fcntl` — with no
    // allocation and no locking.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            if vsock_fd != PROTOCOL_FD {
                if libc::dup2(vsock_fd, PROTOCOL_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(vsock_fd);
            }
            // `dup2` clears `CLOEXEC` only when it actually copies; when the
            // dial already sits on fd 3 the flag set at `socket(2)` survives
            // and must be cleared explicitly either way.
            if libc::fcntl(PROTOCOL_FD, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The seed fd keeps its own number (named to the child by env
            // var, not a fixed slot), so only its `CLOEXEC` needs clearing.
            let flags = libc::fcntl(seed_fd, libc::F_GETFD);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(seed_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .map_err(|e| format!("hatch: could not start the child engine: {e}"))?;
    let pid = child.id();
    // The child exists, so the host may now be told: the ack is written after
    // the spawn and before the first frame, and it is all the host has to go
    // on before it names this child on its roster.
    let ack = vsock.write_all(&[crate::transport::HATCH_ACK]);
    // The parent must not hold either the dial or the child's seed end open,
    // or the child's death would never read as EOF on the ends this process
    // keeps — `vsock` drops here as `WireTransport::new`'s own `engine` end
    // does, and `child_seed` beside it.
    drop(vsock);
    drop(child_seed);

    // Recorded before the ack is reported on, so a child that started is
    // reaped even when the host never heard of it.
    table()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(Hatched {
            child: ChildHandle::from_std(child),
            seed: WireChannel::from_stream(parent_seed),
        });
    ack.map_err(|e| {
        format!("hatch: the child engine started, but the host could not be told so: {e}")
    })?;
    Ok(pid)
}

/// Guest-side application of a wire seed: called from `engine_session` once
/// the installer has booted `shell`, if [`RAL_ENGINE_SEED_FD_ENV`] names an
/// fd. Hydrates scope and context exactly as `child_eval::eval_request`
/// does, then narrows `shell`'s capabilities to `own ⊓ resolve_base(grant,
/// cwd)` through whichever policy [`set_grant_narrower`] registered.
///
/// # Errors
/// Returns a sentence naming whichever step failed: the fd, the read, the
/// decode, or an unregistered grant policy — a wire-seeded child is refused
/// rather than admitted with no way to enforce its ceiling.
pub(crate) fn apply_seed(fd: &str, shell: &mut Shell) -> Result<(), String> {
    let fd: RawFd = fd
        .parse()
        .map_err(|_| format!("{RAL_ENGINE_SEED_FD_ENV} is not an fd: {fd:?}"))?;
    // SAFETY: the parent placed exactly one open fd here, named by this env
    // var and touched by nothing else in this process.
    let mut stream = unsafe { UnixStream::from_raw_fd(fd) };
    let seed: EngineSeed = crate::subprocess_codec::read_frame(&mut stream)
        .map_err(|e| format!("hatch: failed to read the engine seed: {e}"))?
        .ok_or_else(|| "hatch: the seed channel closed before a seed arrived".to_string())?;

    let dec = WireDecoder::for_shell(shell, &seed.scope_table)
        .map_err(|e| format!("hatch: the seed's scope failed to decode: {}", e.message))?;
    install_shell_mobile(seed.mobile, shell, &dec)
        .map_err(|e| format!("hatch: the seed's context failed to decode: {}", e.message))?;
    shell.mobile.scope = seed
        .captured
        .into_runtime(&dec)
        .map_err(|e| format!("hatch: the seed's scope failed to decode: {}", e.message))?;

    let narrow = GRANT_NARROWER.get().ok_or_else(|| {
        "hatch: this engine has no grant-narrowing policy installed; a wire-seeded child cannot \
         be admitted"
            .to_string()
    })?;
    let own = shell.mobile().context.grants.effective();
    let cwd = shell.cwd();
    let narrowed = narrow(&own, &seed.grant, &cwd.to_string_lossy())?;
    shell.push_session_capabilities(narrowed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subprocess::bare_child_shell;
    use crate::types::{Fork, Mooring, Nursery, Value};

    /// A grant narrower that just meets `own` against a fixed floor, so
    /// tests need no exarch-shaped base vocabulary.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "must match GrantNarrower's fn-pointer signature, which can genuinely refuse"
    )]
    fn deny_net(own: &Capabilities, _grant: &str, _cwd: &str) -> Result<Capabilities, String> {
        Ok(own.clone().meet(Capabilities {
            net: Some(false),
            ..Capabilities::default()
        }))
    }

    #[test]
    fn hatch_over_a_socketpair_seeds_a_fresh_engine_process() {
        let mut parent = Shell::new(crate::io::TerminalState::default());
        parent
            .mobile
            .scope
            .set("greeting".to_string(), Value::String("hi".to_string()));

        let nursery = Nursery::default();
        let mooring = Mooring {
            fork: Some(Fork::Park(nursery.clone())),
            ..Mooring::adrift()
        };
        let id = parent
            .fork_into_nursery(&mooring)
            .expect("a nursery is installed");
        let nursery_shell = nursery.adopt(id).expect("adopt the parked fork");

        let (a, b) = UnixStream::pair().expect("socketpair standing in for the host's connection");
        let a: OwnedFd = a.into();
        let peer_thread = std::thread::spawn(move || {
            let mut b = b;
            let mut ack = [0u8; 1];
            b.read_exact(&mut ack).expect("read the ack");
            ack[0]
        });

        let pid = hatch_over(a, &pack_seed(&nursery_shell, "read-only".into()).unwrap())
            .expect("hatch over a socketpair");

        assert_eq!(
            peer_thread.join().expect("peer thread"),
            crate::transport::HATCH_ACK,
            "the host learns of the child only by its ack"
        );

        assert!(
            recorded(pid),
            "hatch_over must record the child before returning"
        );
    }

    const TOKEN: u64 = 0xdead_beef_1234_5678;

    /// The table is process-global and these tests run beside each other, so
    /// each looks for its own child rather than counting.
    fn recorded(pid: u32) -> bool {
        table()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|hatched| hatched.child.id() == pid)
    }

    fn a_seed() -> EngineSeed {
        let shell = Shell::new(crate::io::TerminalState::default());
        packed_seed(&shell, "read-only".to_string()).expect("pack a seed")
    }

    /// A `UnixListener` stands in for the guest's bound vsock port.
    fn listening(dir: &tempfile::TempDir) -> (std::path::PathBuf, HatchListener) {
        let path = dir.path().join("spawn");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind a listener");
        let hatching =
            hatch_listener(listener.into(), TOKEN, a_seed()).expect("start the listener thread");
        (path, hatching)
    }

    #[test]
    fn the_host_dials_in_and_only_the_token_hatches_a_child() {
        let dir = tempfile::tempdir().expect("a directory for the listening socket");
        let (path, hatching) = listening(&dir);

        let mut rogue = UnixStream::connect(&path).expect("a rogue dial");
        rogue
            .write_all(&0u64.to_le_bytes())
            .expect("a wrong token, written first");
        let mut end = [0u8; 1];
        assert_eq!(
            rogue.read(&mut end).ok(),
            Some(0),
            "a wrong token costs the dialler its connection, and nothing else"
        );

        let mut host = UnixStream::connect(&path).expect("the host's dial");
        host.write_all(&TOKEN.to_le_bytes()).expect("the token");
        let mut ack = [0u8; 1];
        host.read_exact(&mut ack).expect("the ack");
        assert_eq!(ack[0], crate::transport::HATCH_ACK);

        let pid = hatching.join().expect("a child hatched");
        assert!(recorded(pid), "the thread hatched before it acked");
    }

    #[test]
    fn a_cancelled_listener_has_no_reason_of_its_own() {
        let dir = tempfile::tempdir().expect("a directory for the listening socket");
        let (_path, hatching) = listening(&dir);
        hatching.cancel();
        match hatching.join() {
            Err(Unhatched::Cancelled) => {}
            other => panic!("a woken listener must have nothing to say, but said {other:?}"),
        }
    }

    #[test]
    fn apply_seed_hydrates_scope_and_narrows_capabilities() {
        GRANT_NARROWER.get_or_init(|| deny_net);

        let mut parent = Shell::new(crate::io::TerminalState::default());
        parent.mobile.scope.set("kept".to_string(), Value::Int(7));
        let seed = pack_seed(&parent, "confined".to_string()).expect("pack seed");

        let (mut writer, reader) = UnixStream::pair().expect("socketpair");
        std::thread::spawn(move || {
            crate::subprocess_codec::write_frame(&mut writer, &seed).expect("write seed");
        });

        let mut shell = bare_child_shell();
        let reader_fd = {
            use std::os::fd::IntoRawFd;
            reader.into_raw_fd()
        };
        apply_seed(&reader_fd.to_string(), &mut shell).expect("apply seed");

        assert_eq!(shell.mobile.scope.get("kept"), Some(&Value::Int(7)));
        assert_eq!(
            shell.mobile().context.grants.effective().net,
            Some(false),
            "the registered narrower's floor must land on the hydrated shell"
        );
    }
}
