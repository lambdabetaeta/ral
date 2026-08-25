//! The hatch: a parent engine spawning a child engine onto a connection it
//! was handed, and that child hydrating the seed it was spawned with.
//!
//! The caller binds a socket for one spawn and passes it to
//! [`listen_for_hatch`]. The listener thread checks the dialler's eight token
//! bytes and hands the connection to [`hatch_over`], which re-execs this
//! binary with the dial on fd 3 and a seed socketpair named by
//! `RAL_ENGINE_SEED_FD`, writes the one framed [`EngineSeed`] while the child
//! drains it, and answers [`crate::transport::HATCH_ACK`]: a peer that hears
//! the ack has a live child holding its whole seed. [`seed_from_env`] and
//! [`apply_seed`] are the other end, narrowing the child through the
//! [`GrantNarrower`] its [`crate::engine::EngineInstaller`] carries — core has
//! no grant vocabulary of its own.
//!
//! Nothing here names a transport: the listening socket is the caller's, and
//! the tests stand a `UnixListener` and `UnixStream` pairs in for it. Only a
//! Linux guest reaches this at all, so a plain non-Linux build sees the whole
//! chain as unreachable — a fact stated once by the blanket allow rather than
//! on every item it covers.
#![cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

use crate::child_eval::{EngineSeed, pack_seed};
use crate::process::ChildHandle;
use crate::serial::WireDecoder;
use crate::subprocess::install_shell_mobile;
use crate::types::{Capabilities, Shell};

/// The engine's protocol socket lands on this descriptor, exactly as
/// `run_engine` expects (`core/src/engine.rs`) and `ral-daemon` already
/// spawns onto (`PROTOCOL_FD`).
const PROTOCOL_FD: RawFd = 3;

/// The env var carrying the seed socketpair's fd number — the pipeline
/// helpers' env-named-fd pattern (`runtime/pipeline/helper.rs`), not a second
/// magic number. Private: [`seed_from_env`] is the only reader, and it strikes
/// the name as it takes the fd.
const RAL_ENGINE_SEED_FD_ENV: &str = "RAL_ENGINE_SEED_FD";

/// A hatched child's argv, carried by the hatch so the tests can name their
/// own child without a `cfg(test)` in the spawn.
type Recipe = &'static [&'static str];

#[cfg(target_os = "linux")]
const ENGINE: Recipe = &["--engine"];

/// `own`, the grant tag, the cwd — in, a narrowed [`Capabilities`] out.
///
/// The narrowing is `own ⊓ resolve_base(grant, cwd)`, evaluated where the
/// host's own capability vocabulary lives, since core carries no base-tag
/// lexicon. A field of [`crate::engine::EngineInstaller`] rather than a registered
/// hook: an installer is chosen at `Attach`, before [`apply_seed`] runs, so
/// the policy can be demanded of every host that dresses an engine instead of
/// left in a slot one of them might forget to fill.
pub type GrantNarrower = fn(&Capabilities, &str, &str) -> Result<Capabilities, String>;

/// The process-global hatch table. No thread, no signal handler: a hatch
/// sweeps it on entry, and `teardown` sweeps it once more as the engine
/// exits, so a zombie between sweeps is inert and bounded by the table.
static HATCHED: OnceLock<Mutex<Vec<ChildHandle>>> = OnceLock::new();

fn table() -> &'static Mutex<Vec<ChildHandle>> {
    HATCHED.get_or_init(|| Mutex::new(Vec::new()))
}

/// Reap every hatched child that has exited; leave the rest running. Only
/// `waitpid` can tell them apart: a child closes its seed channel when it
/// hydrates, not when it dies.
fn sweep_hatched() {
    let mut table = table()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    table.retain_mut(|child| !matches!(child.try_wait_handling_stop(None, false), Ok(Some(_))));
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
    thread: JoinHandle<Result<(), Unhatched>>,
}

impl HatchListener {
    /// Wake the thread out of whichever poll it is parked in, so a caller
    /// whose enquiry was refused can always have its thread back.
    pub fn cancel(&self) {
        let _ = (&self.wake).write(&[0]);
    }

    /// Wait for the thread. A hatched child is the table's to reap, so there
    /// is nothing to answer with.
    ///
    /// # Errors
    /// [`Unhatched::Failed`] if the thread got as far as a reason of its own;
    /// [`Unhatched::Cancelled`] if [`Self::cancel`] reached it first.
    pub fn join(self) -> Result<(), Unhatched> {
        self.thread.join().unwrap_or_else(|_| {
            Err(Unhatched::Failed(
                "hatch: the thread listening for the host's dial panicked".to_string(),
            ))
        })
    }
}

/// Wait on `listener` — a socket the caller has already bound and listened on
/// — for the one dial that hatches a child, and answer with the thread now
/// waiting.
///
/// `token` is the eight bytes the dialler must write first. `shell` is packed
/// here, on the caller's own thread: the seed is all the listener thread ever
/// holds of the parent's session, and a `Shell` never leaves this one.
///
/// # Errors
/// Returns a sentence if the shell will not pack, the wake pipe could not be
/// opened, or the listener thread could not be started.
#[cfg(target_os = "linux")]
pub fn listen_for_hatch(
    listener: OwnedFd,
    token: u64,
    shell: &Shell,
    grant: String,
) -> Result<HatchListener, String> {
    hatch_listener(listener, token, packed_seed(shell, grant)?, ENGINE)
}

/// [`listen_for_hatch`] with the recipe and the packed seed exposed, so the
/// tests can hatch a child of their own naming.
fn hatch_listener(
    listener: OwnedFd,
    token: u64,
    seed: EngineSeed,
    recipe: Recipe,
) -> Result<HatchListener, String> {
    let (woken, wake) = io::pipe()
        .map_err(|e| format!("hatch: could not open the pipe that wakes the listener: {e}"))?;
    let thread = std::thread::Builder::new()
        .name("ral-hatch-listener".to_string())
        .spawn(move || await_dial(&listener, &woken, token, &seed, recipe))
        .map_err(|e| {
            format!("hatch: could not start the thread that waits for the host's dial: {e}")
        })?;
    Ok(HatchListener { wake, thread })
}

/// The listener thread's whole life: accept until the dial bearing the token
/// arrives, then hatch onto it.
///
/// Two kinds of poll site and no clock: the listener, then every partial read
/// of a token. Neither blocks without the wake end in its set, which is what
/// makes the caller's join safe. The one wait past them is [`send_seed`]'s,
/// bounded rather than woken — by then a child exists — and the host's own wait
/// on the ack carries the transport's deadline over all of it.
fn await_dial(
    listener: &OwnedFd,
    woken: &io::PipeReader,
    token: u64,
    seed: &EngineSeed,
    recipe: Recipe,
) -> Result<(), Unhatched> {
    loop {
        wait_readable(listener, woken)?;
        let connection = accept(listener)
            .map_err(|e| Unhatched::Failed(format!("hatch: could not accept a dial: {e}")))?;
        let mut stream = UnixStream::from(connection);
        let Some(claim) = read_claim(&mut stream, woken)? else {
            continue;
        };
        if u64::from_le_bytes(claim) != token {
            // Whoever this was did not know the token, so it is not the peer
            // we were told to expect. Losing costs it only this connection.
            continue;
        }
        return hatch_over(stream.into(), seed, recipe)
            .map(|_pid| ())
            .map_err(Unhatched::Failed);
    }
}

/// Read exactly one token without ever waiting on the dial alone. A peer may
/// send any proper prefix and then stop; cancellation must still recover the
/// listener thread. Such a peer does hold the accept loop meanwhile, so it can
/// deny this one spawn — accepted, since the host's own deadline ends it and
/// the next enquiry binds a fresh port.
fn read_claim(
    stream: &mut UnixStream,
    woken: &io::PipeReader,
) -> Result<Option<[u8; 8]>, Unhatched> {
    let mut claim = [0u8; 8];
    let mut filled = 0;
    while filled < claim.len() {
        wait_readable(stream, woken)?;
        match stream.read(&mut claim[filled..]) {
            Ok(0) => return Ok(None),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Ok(None),
        }
    }
    Ok(Some(claim))
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

/// `accept(2)` with the peer's address discarded — nothing here has any use
/// for it, and std's `UnixListener` would refuse to read an `AF_VSOCK` peer as
/// a path, which is the whole reason this is hand-rolled.
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
fn hatch_over(connection: OwnedFd, seed: &EngineSeed, recipe: Recipe) -> Result<u32, String> {
    sweep_hatched();

    let mut dial = UnixStream::from(connection);
    let (mut parent_seed, child_seed) =
        UnixStream::pair().map_err(|e| format!("hatch: failed to open the seed channel: {e}"))?;
    // The child is draining before the first byte goes out, so a seed larger
    // than the socketpair's buffer crosses instead of wedging both sides.
    let mut child = ChildHandle::from_std(spawn_engine(recipe, &dial, child_seed)?);
    let pid = child.id();
    if let Err(e) = send_seed(&mut parent_seed, seed) {
        // Half a frame leaves the child blocked in its own read, and a child
        // with no seed can never attach: it is killed here rather than
        // recorded for a sweep that would wait forever to notice it.
        let _ = child.kill();
        let _ = child.reap();
        return Err(e);
    }
    // The child exists and holds its seed, so the peer may now be told: the
    // ack is the byte before the first protocol frame, and it is all the peer
    // has to go on before it names this child on its roster.
    let ack = dial.write_all(&[crate::transport::HATCH_ACK]).map_err(|e| {
        format!("hatch: the child engine started, but the host could not be told so: {e}")
    });
    // The peer's end must read the child's death as EOF, so this process keeps
    // no copy of the dial — it drops here as `WireTransport::new`'s own
    // `engine` end does.
    drop(dial);

    // Recorded before the ack is reported on, so a child that started is
    // reaped even when the host never heard of it.
    table()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(child);
    ack?;
    Ok(pid)
}

/// Start the child engine: the dial on [`PROTOCOL_FD`], the seed channel on an
/// fd named to it by env var.
///
/// Taking `child_seed` by value is the whole discipline: it is closed as this
/// returns, so no caller can write the seed while a reading end still lives in
/// this process — which would leave that write blocked on a dead child instead
/// of failing it.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:hatch-spawn] re-execs the current engine binary as a hatched child over the connection a peer just dialled — infrastructure handoff exactly like WireTransport::new's engine-spawn door, not model turn-time I/O"
)]
fn spawn_engine(
    recipe: Recipe,
    dial: &UnixStream,
    child_seed: UnixStream,
) -> Result<Child, String> {
    let current_exe = std::env::current_exe()
        .map_err(|e| format!("hatch: could not resolve this engine's own executable path: {e}"))?;
    let mut cmd = Command::new(current_exe);
    cmd.args(recipe);
    cmd.env(RAL_ENGINE_SEED_FD_ENV, child_seed.as_raw_fd().to_string());
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    let dial_fd = dial.as_raw_fd();
    let seed_fd = child_seed.as_raw_fd();
    // SAFETY: the closure runs between `fork` and `exec` and calls only
    // async-signal-safe syscalls — `dup2`, `close`, `fcntl` — with no
    // allocation and no locking.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            if dial_fd != PROTOCOL_FD {
                if libc::dup2(dial_fd, PROTOCOL_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(dial_fd);
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
        .map_err(|e| format!("hatch: could not start the child engine: {e}"));
    // Closed here and nowhere else. The child holds its own copy across the
    // exec, and one kept in this process would leave the seed write blocked on
    // a dead child instead of failing it — which is why this end is taken by
    // value rather than borrowed.
    drop(child_seed);
    child
}

/// Hand the child its one frame, bounded by the same stall the engine allows
/// its own protocol writes ([`crate::engine::HOST_SILENCE_DEADLINE`]) — no new
/// number, and none of the listener thread's poll sites: by the time a seed is
/// crossing, the child exists, so what this wait needs is a bound and not the
/// wake pipe's cancel.
fn send_seed(parent_seed: &mut UnixStream, seed: &EngineSeed) -> Result<(), String> {
    parent_seed
        .set_write_timeout(Some(crate::engine::HOST_SILENCE_DEADLINE))
        .map_err(|e| format!("hatch: could not bound the wait for the child's seed: {e}"))?;
    let sent = crate::subprocess_codec::write_frame(parent_seed, seed);
    // The bound belonged to that one write; the end this process keeps outlives
    // it, and is only ever polled for the child's death.
    let _ = parent_seed.set_write_timeout(None);
    sent.map_err(|e| format!("hatch: the child engine started, but its seed could not be sent: {e}"))
}

/// The wire seed this engine was hatched with, if it was hatched at all.
///
/// `engine_session` calls this before it waits for `Attach`: the parent writes
/// the frame only once this process exists, so a child that waited on the host
/// first would wedge both sides on any seed larger than the socket's buffer.
/// The fd's name is struck from the environment as the fd is taken, so no
/// descendant inherits a number that has stopped being one.
///
/// # Errors
/// Returns a sentence naming an unreadable name, an invalid fd, a failed read,
/// or an early EOF.
pub(crate) fn seed_from_env() -> Result<Option<EngineSeed>, String> {
    let named = std::env::var(RAL_ENGINE_SEED_FD_ENV);
    // SAFETY: engine startup, before this process has a second thread.
    unsafe { std::env::remove_var(RAL_ENGINE_SEED_FD_ENV) };
    let fd: RawFd = match &named {
        Ok(fd) => fd
            .parse()
            .map_err(|_| format!("{RAL_ENGINE_SEED_FD_ENV} is not an fd: {fd:?}"))?,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(format!("{RAL_ENGINE_SEED_FD_ENV} is not a valid fd number"));
        }
    };
    // SAFETY: the parent placed exactly one open fd here, named by this env
    // var and touched by nothing else in this process.
    read_seed(unsafe { UnixStream::from_raw_fd(fd) }).map(Some)
}

/// The one framed seed on `channel`, which is closed as this returns.
fn read_seed(mut channel: UnixStream) -> Result<EngineSeed, String> {
    crate::subprocess_codec::read_frame(&mut channel)
        .map_err(|e| format!("hatch: failed to read the engine seed: {e}"))?
        .ok_or_else(|| "hatch: the seed channel closed before a seed arrived".to_string())
}

/// Application of a seed already taken: called from `engine_session` once the
/// installer has booted `shell`. Hydrates scope and context exactly as
/// `child_eval::eval_request` does, then narrows `shell`'s capabilities
/// through `narrow`, the [`GrantNarrower`] that installer carries.
///
/// # Errors
/// Returns a sentence naming a decode failure, or whatever `narrow` refuses
/// with — a wire-seeded child is refused rather than admitted above its
/// ceiling.
pub(crate) fn apply_seed(
    seed: EngineSeed,
    shell: &mut Shell,
    narrow: GrantNarrower,
) -> Result<(), String> {
    let dec = WireDecoder::for_shell(shell, &seed.scope_table)
        .map_err(|e| format!("hatch: the seed's scope failed to decode: {}", e.message))?;
    install_shell_mobile(seed.mobile, shell, &dec)
        .map_err(|e| format!("hatch: the seed's context failed to decode: {}", e.message))?;
    shell.mobile.scope = seed
        .captured
        .into_runtime(&dec)
        .map_err(|e| format!("hatch: the seed's scope failed to decode: {}", e.message))?;

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
    use crate::types::Value;

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

    /// The libtest binary has no `--engine`, so a hatched child is one exact
    /// test performing the child's half of the fd agreement.
    const FIXTURE: Recipe = &[
        "--exact",
        "hatch::tests::hatched_child_fixture",
        "--nocapture",
    ];

    /// The byte the host asks fd 3 with, and the child answers.
    const PROBE: u8 = b'?';

    /// Production accepts its dial onto a high fd while fd 3 holds the parent
    /// engine's own wire, so only `dup2` puts the dial where the child looks.
    /// A pair left on fd 3 would be inherited either way, and attest nothing.
    fn well_clear_of_fd_3(dial: UnixStream) -> UnixStream {
        // SAFETY: `F_DUPFD` on a descriptor this call owns; the lowest free fd
        // at or above 100, adopted below.
        let raw = unsafe { libc::fcntl(dial.as_raw_fd(), libc::F_DUPFD, 100) };
        assert!(raw >= 100, "the dial must be moved clear of fd 3");
        drop(dial);
        // SAFETY: `raw` is a fresh descriptor this call owns.
        unsafe { UnixStream::from_raw_fd(raw) }
    }

    /// The child half of `hatch_over`'s fd agreement: an ordinary run has no
    /// seed fd and returns, a re-exec drains the whole seed, answers the probe
    /// on fd 3, and lives until the host hangs up.
    #[test]
    fn hatched_child_fixture() {
        let _seed = match seed_from_env() {
            Ok(None) => return,
            Ok(Some(seed)) => seed,
            Err(msg) => panic!("the re-exec child must read its seed: {msg}"),
        };
        // SAFETY: `hatch_over` placed the one protocol connection on fd 3 for
        // this process, exactly as `run_engine` expects.
        let mut protocol = unsafe { UnixStream::from_raw_fd(PROTOCOL_FD) };
        let mut probe = [0u8; 1];
        protocol
            .read_exact(&mut probe)
            .expect("the host's probe on fd 3");
        protocol.write_all(&probe).expect("the answer to it");
        let _ = protocol.read(&mut probe);
    }

    /// The whole hatch through a real re-exec of this binary, with a seed
    /// larger than the socketpair can hold: it crosses only because the child
    /// is draining while the parent writes, which is the one claim here that
    /// no reading of the code can settle. The probe settles a second: fd 3 in
    /// that child is the dial, and only that child can answer on it.
    #[test]
    fn a_seed_outgrowing_the_channel_crosses_to_a_real_child() {
        let mut parent = Shell::new(crate::io::TerminalState::default());
        parent.mobile.scope.set(
            "larger-than-a-socket-buffer".to_string(),
            Value::String("x".repeat(2 * 1024 * 1024)),
        );
        let seed = packed_seed(&parent, "read-only".to_string()).expect("pack a seed");

        let (dial, host) = UnixStream::pair().expect("a socketpair for the host's dial");
        let dial = well_clear_of_fd_3(dial);
        let dialled = std::thread::spawn(move || {
            let mut host = host;
            // Bounded, so a silent child fails this test rather than hanging it.
            host.set_read_timeout(Some(crate::engine::HOST_SILENCE_DEADLINE))
                .expect("bound the host's own wait");
            let mut ack = [0u8; 1];
            host.read_exact(&mut ack).expect("the ack");
            host.write_all(&[PROBE]).expect("a probe for the child");
            let mut answer = [0u8; 1];
            host.read_exact(&mut answer).expect("the child's answer");
            (host, ack[0], answer[0])
        });

        let pid = hatch_over(dial.into(), &seed, FIXTURE).expect("hatch over a socketpair");
        // Held: the child lives as long as this end of the dial.
        let (_host, ack, answer) = dialled.join().expect("the host's thread");
        assert_eq!(
            (ack, answer),
            (crate::transport::HATCH_ACK, PROBE),
            "the host learns of the child by its ack, and of its fd 3 by its answer"
        );
        assert!(recorded(pid), "a started child must be recorded for reaping");

        sweep_hatched();
        assert!(
            recorded(pid),
            "this child has hydrated, so its seed channel is closed — but it runs, and only its \
             exit may retire it"
        );
    }

    /// The table is process-global and these tests run beside each other, so
    /// each looks for its own child rather than counting.
    fn recorded(pid: u32) -> bool {
        table()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|child| child.id() == pid)
    }

    const TOKEN: u64 = 0xdead_beef_1234_5678;

    fn a_seed() -> EngineSeed {
        let shell = Shell::new(crate::io::TerminalState::default());
        packed_seed(&shell, "read-only".to_string()).expect("pack a seed")
    }

    /// A `UnixListener` stands in for the socket the caller binds.
    fn listening(dir: &tempfile::TempDir) -> (std::path::PathBuf, HatchListener) {
        let path = dir.path().join("spawn");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind a listener");
        let hatching = hatch_listener(listener.into(), TOKEN, a_seed(), FIXTURE)
            .expect("start the listener thread");
        (path, hatching)
    }

    #[test]
    fn a_wrong_token_costs_only_its_own_connection() {
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
            "a wrong token costs the dialler its connection"
        );

        hatching.cancel();
        match hatching.join() {
            Err(Unhatched::Cancelled) => {}
            other => panic!("the listener must still be waiting, but got {other:?}"),
        }
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

    /// Straight at [`read_claim`]: through the listener thread there is no
    /// moment a test can name, so byte and cancel are both in place before the
    /// call and the poll's own order is what answers.
    #[test]
    fn a_partial_token_cannot_pin_cancellation() {
        let (mut rogue, mut dialled) = UnixStream::pair().expect("a socketpair for the dial");
        rogue
            .write_all(&TOKEN.to_le_bytes()[..1])
            .expect("a proper token prefix");
        let (woken, wake) = io::pipe().expect("a wake pipe");
        (&wake).write_all(&[0]).expect("a caller that has given up");

        match read_claim(&mut dialled, &woken) {
            Err(Unhatched::Cancelled) => {}
            other => panic!("a partial token must not pin cancellation, but got {other:?}"),
        }
    }

    #[test]
    fn apply_seed_hydrates_scope_and_narrows_capabilities() {
        let mut parent = Shell::new(crate::io::TerminalState::default());
        parent.mobile.scope.set("kept".to_string(), Value::Int(7));
        let seed = pack_seed(&parent, "confined".to_string()).expect("pack seed");

        let (mut writer, reader) = UnixStream::pair().expect("socketpair");
        std::thread::spawn(move || send_seed(&mut writer, &seed).expect("send the seed"));

        let mut shell = bare_child_shell();
        apply_seed(read_seed(reader).expect("read seed"), &mut shell, deny_net)
            .expect("apply seed");

        assert_eq!(shell.mobile.scope.get("kept"), Some(&Value::Int(7)));
        assert_eq!(
            shell.mobile().context.grants.effective().net,
            Some(false),
            "the installer's narrower must land its floor on the hydrated shell"
        );
    }
}
