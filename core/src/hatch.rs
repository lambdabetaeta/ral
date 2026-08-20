//! The guest hatch: a parent engine spawning a child engine over a fresh
//! vsock dial, and that child engine hydrating the seed it was spawned with.
//!
//! `hatch` mirrors [`crate::transport::WireTransport::new`]'s re-exec shape,
//! with `ral-daemon`'s own fd discipline: dial the host, write a 16-byte
//! preamble identifying the dial to the token the caller was already handed,
//! seed a fresh socketpair with one framed [`EngineSeed`], and spawn
//! `current_exe --engine` with the dial on fd 3 and the seed's fd named by
//! `RAL_ENGINE_SEED_FD`. [`apply_seed`] is the other end: called from
//! `engine_session` once a freshly booted shell exists, it hydrates the
//! seed's scope and context, then narrows the shell's capabilities to the
//! seed's validated grant through whichever host installed
//! [`set_grant_narrower`] — core itself carries no capability vocabulary of
//! its own to resolve a grant tag against.
//!
//! The dial alone — [`crate::vsock::dial_host`] — is Linux-only, since
//! `AF_VSOCK` only means something inside a real guest. Everything else here
//! is plain Unix process/socket plumbing, exercised in tests by handing
//! [`hatch_over`] one end of a `UnixStream` pair standing in for the vsock
//! dial, the same vehicle the wire seat's own tests already re-exec
//! `--engine` over.
//!
//! `hatch` itself is Linux-only, and everything below it that exists only to
//! serve that one call or this file's own tests is therefore unreachable in
//! a plain non-Linux, non-test build — an accurate fact about this
//! milestone's shape, not a bug, so it is stated once here rather than
//! peppering every such item with its own `#[allow(dead_code)]`.
#![cfg_attr(not(any(target_os = "linux", test)), allow(dead_code, unused_imports))]

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::child_eval::{EngineSeed, pack_seed};
use crate::process::ChildHandle;
use crate::serial::WireDecoder;
use crate::subprocess::install_shell_mobile;
use crate::types::{Capabilities, Shell};
#[cfg(target_os = "linux")]
use crate::types::{Mooring, NurseryId};
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

/// Dial the host's agent port, and spawn a child engine seeded from `shell`
/// — the nursery-parked shell `agent-start`'s wire arm parked, already
/// scrubbed by [`Shell::fork_into_nursery`].
///
/// Answers the child's pid, so a caller whose later `agent-hatched` enquiry
/// is refused (a timeout, a dead desk) can [`kill_hatched`] this one child
/// rather than leave it dialling into silence.
///
/// # Errors
/// Returns a sentence describing whichever step failed: the dial, the seed
/// socketpair, the seed encode, or the spawn.
#[cfg(target_os = "linux")]
pub fn hatch(host_port: u32, token: u64, shell: &Shell, grant: String) -> Result<u32, String> {
    use crate::types::Break;

    let vsock = crate::vsock::dial_host(host_port)
        .map_err(|e| format!("hatch: could not dial the host's agent port {host_port}: {e}"))?;
    let seed = pack_seed(shell, grant).map_err(|b| match b {
        Break::Error(e) => format!(
            "hatch: could not serialise the nursery shell: {}",
            e.message
        ),
        Break::Escape(_) => {
            "hatch: could not serialise the nursery shell: unexpected escape".to_string()
        }
    })?;
    hatch_over(vsock, token, &seed)
}

/// [`hatch`], reading its own seed straight out of `mooring`'s nursery
/// instead of taking an already-adopted `Shell`.
///
/// The whole other half of `Shell::fork_into_nursery`, wrapped in one call
/// because a nursery slot is this crate's own private field and a host
/// builtin cannot reach it any other way.
///
/// # Errors
/// Returns a sentence if `id` names no parked fork, or [`hatch`] itself
/// fails.
#[cfg(target_os = "linux")]
pub fn hatch_from_nursery(
    host_port: u32,
    token: u64,
    mooring: &Mooring,
    id: NurseryId,
    grant: String,
) -> Result<u32, String> {
    let shell = mooring
        .nursery
        .as_ref()
        .and_then(|nursery| nursery.adopt(id))
        .ok_or_else(|| "hatch: no forked session parked under this id".to_string())?;
    hatch(host_port, token, &shell, grant)
}

/// The portable core of [`hatch`]: everything past the vsock dial itself.
/// `connection` stands in for the dial in tests.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:hatch-spawn] re-execs the current engine binary as a hatched child over a fresh vsock connection — infrastructure handoff exactly like WireTransport::new's engine-spawn door, not model turn-time I/O"
)]
fn hatch_over(connection: OwnedFd, token: u64, seed: &EngineSeed) -> Result<u32, String> {
    sweep_hatched();

    let mut vsock = UnixStream::from(connection);
    let mut preamble = [0u8; 16];
    preamble[..8].copy_from_slice(&crate::hatch_preamble::MAGIC);
    preamble[8..].copy_from_slice(&token.to_le_bytes());
    vsock
        .write_all(&preamble)
        .map_err(|e| format!("hatch: failed to write the agent-port preamble: {e}"))?;

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
    // The parent must not hold either the dial or the child's seed end open,
    // or the child's death would never read as EOF on the ends this process
    // keeps — `vsock` drops here as `WireTransport::new`'s own `engine` end
    // does, and `child_seed` beside it.
    drop(vsock);
    drop(child_seed);

    table()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(Hatched {
            child: ChildHandle::from_std(child),
            seed: WireChannel::from_stream(parent_seed),
        });
    Ok(pid)
}

/// Kill and reap the hatched child whose pid this is.
///
/// [`hatch`]'s own caller, when a later `agent-hatched` enquiry is refused (a
/// timeout, a dead desk) after the hatch itself already succeeded: the child
/// is left dialling into silence otherwise, since nothing else in this engine
/// ever looks for it again.
pub fn kill_hatched(pid: u32) {
    // Removed under the lock, killed and reaped after it drops: `Hatched`
    // holds a `ChildHandle` whose own teardown must never run while this
    // table's mutex is still held.
    let entry = {
        let mut table = table()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        table
            .iter()
            .position(|h| h.child.id() == pid)
            .map(|pos| table.remove(pos))
    };
    if let Some(mut entry) = entry {
        let _ = entry.child.kill();
        let _ = entry.child.wait_handling_stop(None, false);
    }
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
    use crate::types::{Mooring, Nursery, Value};

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
            nursery: Some(nursery.clone()),
            ..Mooring::adrift()
        };
        let id = parent
            .fork_into_nursery(&mooring)
            .expect("a nursery is installed");
        let nursery_shell = nursery.adopt(id).expect("adopt the parked fork");

        let (a, b) = UnixStream::pair().expect("socketpair standing in for the vsock dial");
        let a: OwnedFd = a.into();
        let peer_thread = std::thread::spawn(move || {
            let mut b = b;
            let mut preamble = [0u8; 16];
            std::io::Read::read_exact(&mut b, &mut preamble).expect("read preamble");
            preamble
        });

        hatch_over(
            a,
            0xdead_beef_1234_5678,
            &pack_seed(&nursery_shell, "read-only".into()).unwrap(),
        )
        .expect("hatch over a socketpair");

        let preamble = peer_thread.join().expect("peer thread");
        assert_eq!(&preamble[..8], &crate::hatch_preamble::MAGIC);
        assert_eq!(
            u64::from_le_bytes(preamble[8..].try_into().unwrap()),
            0xdead_beef_1234_5678
        );

        assert_eq!(
            table()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1,
            "hatch_over must record the child before returning"
        );
        teardown_hatched();
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
