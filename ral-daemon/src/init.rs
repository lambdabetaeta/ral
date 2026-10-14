//! The boot narrative itself: the one impure function, and the end it comes
//! to.
//!
//! Everything this module does is a syscall in a fixed order.  The decisions
//! it performs live in [`crate::boot`], [`crate::mounts`], [`crate::engine`],
//! [`crate::net`], [`crate::pump`], and [`crate::reap`], where they are data
//! and can be tested; here they are only carried out.

#![allow(
    clippy::disallowed_macros,
    reason = "[silent:guest-main] the guest daemon cannot depend on core, so no `errln!`"
)]
use std::convert::Infallible;
use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, getpid, kill_process_group};
use rustix::system::{RebootCommand, reboot};
use rustix::time::{ClockId, Timespec, clock_settime};

use crate::boot::{self, Boot, Export, Net};
use crate::reap::{self, Waking};
use crate::{engine, mounts, net, pump, sysctl, vsock};

/// How long the engine is given to finish after being asked to stop, before
/// it is killed.  Long enough for a run to abandon its work and flush the
/// workspace; short enough that a wedged guest does not hold a user's
/// afternoon.
const GRACE: Duration = Duration::from_secs(5);

/// How long the factory waits before retrying an accept that failed for want
/// of a resource.
const RESOURCE_BACKOFF: Duration = Duration::from_millis(100);

/// How often the grace period looks to see whether the engines have gone.
const PULSE: Duration = Duration::from_millis(50);

/// How long a dead engine's killed cgroups are given to empty before their
/// removal is abandoned.
const SETTLE: Duration = Duration::from_millis(500);

/// Every engine the factory has started and not yet reaped, and whether it
/// may start more. Shared with the accept thread; `halt` closes admission
/// and snapshots in one lock, so every engine's process group is asked to
/// stop and none starts after.
#[derive(Default)]
struct Fleet {
    pids: Vec<Pid>,
    open: bool,
}

impl Fleet {
    fn new(root: Pid) -> Self {
        Self {
            pids: vec![root],
            open: true,
        }
    }

    /// Run `spawn` and record its pid, unless admission is closed.
    fn admit<E>(&mut self, spawn: impl FnOnce() -> Result<Pid, E>) -> Option<Result<Pid, E>> {
        self.open
            .then(|| spawn().inspect(|&pid| self.pids.push(pid)))
    }

    /// Refuse every later `admit`; the engines so far.
    fn close(&mut self) -> Vec<Pid> {
        self.open = false;
        self.pids.clone()
    }

    /// Drop `pid`; whether it was an engine.
    fn forget(&mut self, pid: Pid) -> bool {
        let before = self.pids.len();
        self.pids.retain(|&p| p != pid);
        self.pids.len() != before
    }
}

type Engines = Arc<Mutex<Fleet>>;

/// Why the engine factory died, when it did; read by [`attend`] on the stop
/// signal the factory sends, so that the machine's epitaph names it.
static FACTORY_FAILURE: OnceLock<String> = OnceLock::new();

/// Be the guest's init.
///
/// Returns only when the daemon cannot carry on: on success it never
/// returns at all, because the machine it was running is gone.
///
/// # Errors
/// Returns a sentence explaining what stopped it — this is not our guest,
/// the boot was misconfigured, a filesystem would not mount, the host was
/// not listening, or the engine could not be started.
pub fn serve() -> Result<Infallible, String> {
    // The one exception to the refusal below: the pump (`crate::pump`) is
    // `ral-daemon --pump`, re-executed as a plain child of this same init
    // rather than as pid 1, so it must never reach the is-init check at all.
    // Every other argv — including none — still meets the hard refusal, the
    // same law `boot::Boot::read` applies to an unknown `ral.` key: a
    // spelling this daemon does not recognise is refused, not ignored.
    if std::env::args().nth(1).as_deref() == Some("--pump") {
        return pump::run();
    }

    if !getpid().is_init() {
        return Err(format!(
            "ral-daemon is the init process of a ral guest, not a command. It is what runs as \
             pid 1 inside the virtual machine synod and exarch boot; here it is pid {}, where \
             it would mount over your filesystems and set your machine's clock from a kernel \
             command line. Nothing was done.",
            getpid().as_raw_nonzero()
        ));
    }
    // Before any thread exists, so every thread inherits the block.
    reap::mask(libc::SIG_BLOCK)
        .map_err(|err| format!("could not block the signals the daemon waits on: {err}"))?;

    // `/proc` first: the configuration is read out of it, so it cannot be
    // part of a plan computed from that configuration.
    mounts::procfs().apply()?;

    let cmdline = std::fs::read_to_string("/proc/cmdline")
        .map_err(|err| format!("could not read the kernel command line: {err}"))?;
    let boot = Boot::read(&cmdline)?;

    announce_root();

    clock_settime(
        ClockId::Realtime,
        Timespec {
            tv_sec: boot.epoch,
            tv_nsec: 0,
        },
    )
    .map_err(|err| {
        format!(
            "could not set the guest's clock to the host's ({}): {err}",
            boot.epoch
        )
    })?;

    for filesystem in mounts::plan(&boot) {
        filesystem.apply()?;
    }
    mounts::exec_cgroup()?;
    // Which of the two the host chose is worth one line in its own log: it is
    // the difference between the two hypervisors, and the first thing to know
    // when the workspace misbehaves.
    let export = match &boot.workspace {
        Export::Virtiofs { tag } => format!("virtiofs tag `{tag}`"),
        Export::Plan9 { name, port } => {
            format!("the host's 9p share `{name}`, over vsock port {port}")
        }
    };
    eprintln!(
        "ral-daemon: guest filesystems up; the granted folder is mounted at {} from {export}",
        mounts::WORK
    );

    // Before any external code can run: the jail's whole cross-uid-ptrace
    // argument depends on unprivileged user namespaces being off.
    for setting in sysctl::plan() {
        setting.apply()?;
    }

    // Ctrl-Alt-Del then reaches init as SIGINT, a stop request.
    if let Err(err) = reboot(RebootCommand::CadOff) {
        eprintln!("ral-daemon: could not take Ctrl-Alt-Del's power to reboot the guest: {err}");
    }

    // The control plane is dialled first — this is what fixes the Windows
    // broker's serial accept order and preserves today's readiness
    // semantics — but not handed to the engine yet: the engine is the last
    // thing this function starts, so that the network (if any) is already
    // up before any process exists that could reach it.
    let control = engine::control_plane(boot.port)?;

    let pump = match &boot.net {
        Some(net_config) => Some(bring_up_network(net_config)?),
        None => None,
    };

    let listener = vsock::listen(boot::FACTORY_PORT).map_err(|err| {
        format!(
            "could not listen for the host's engine dials on vsock port {}: {err}. Does the \
             guest have a vsock device?",
            boot::FACTORY_PORT
        )
    })?;

    let root = engine::spawn(&boot, &control)?;
    // The child holds its own copy on fd 3 now; the parent's would only keep
    // the connection alive past the engine's death.
    drop(control);
    eprintln!(
        "ral-daemon: engine running as pid {} on vsock port {}",
        root.as_raw_nonzero(),
        boot.port
    );

    // The root is pushed before the accept thread exists, so it is first.
    let engines: Engines = Arc::new(Mutex::new(Fleet::new(root)));
    std::thread::Builder::new()
        .name("ral-engine-factory".into())
        .spawn({
            let engines = engines.clone();
            move || {
                make_engines(&listener, &boot, &engines);
                // The port stays bound until power-off, lest a guest process take it.
                loop {
                    std::thread::park();
                }
            }
        })
        .map_err(|err| format!("could not start the engine factory thread: {err}"))?;

    halt(&engines, pump, &attend(root, pump, &engines)?)
}

/// How long to wait before retrying a failed `accept`; `None` if the failure
/// is not one a standing listener survives.
fn accept_backoff(err: &io::Error) -> Option<Duration> {
    match err.raw_os_error()? {
        libc::ECONNABORTED | libc::EPROTO | libc::EAGAIN => Some(Duration::ZERO),
        libc::EMFILE | libc::ENFILE | libc::ENOMEM | libc::ENOBUFS => Some(RESOURCE_BACKOFF),
        _ => None,
    }
}

/// The accept thread's whole life: one engine per host dial. A non-host peer
/// is dropped and logged; a failed spawn is logged and its connection
/// dropped, so the host reads EOF and severs. Transient accept errors are
/// retried; any other brings the machine down.
fn make_engines(listener: &OwnedFd, boot: &Boot, engines: &Engines) {
    loop {
        match vsock::accept_from_host(listener) {
            Ok(None) => eprintln!("ral-daemon: refused a dial that was not the host's"),
            Ok(Some(conn)) => {
                // Under the lock, `attend` reaps it only once recorded (std
                // reaps a failed spawn itself) and `halt` cannot miss it.
                let spawned = live(engines).admit(|| engine::spawn(boot, &conn));
                match spawned {
                    None => return eprintln!("ral-daemon: the machine is stopping; dial dropped"),
                    Some(Ok(pid)) => eprintln!(
                        "ral-daemon: engine running as pid {} for the host's dial",
                        pid.as_raw_nonzero()
                    ),
                    Some(Err(err)) => eprintln!("ral-daemon: {err}"),
                }
            }
            Err(err) => match accept_backoff(&err) {
                Some(pause) => {
                    eprintln!("ral-daemon: the engine factory will retry an accept: {err}");
                    std::thread::sleep(pause);
                }
                None => return fail_factory(&err),
            },
        }
    }
}

/// Record why the factory died and send the process a stop signal.  `kill`,
/// never `raise`: a thread-directed signal stays pending on this thread,
/// which blocks it, where [`attend`]'s `sigwaitinfo` never looks.
fn fail_factory(err: &io::Error) {
    let _ = FACTORY_FAILURE.set(format!(
        "the engine factory stopped accepting the host's dials: {err}"
    ));
    let _ = rustix::process::kill_process(getpid(), Signal::TERM);
}

/// Dial the host's net wire, clear the resolver config, bring the `tun` up,
/// and start the pump — everything step 10 of the crate's boot narrative
/// promises, in the order it promises it.
///
/// `/etc/resolv.conf` is emptied rather than left as the image shipped it:
/// a stray `getaddrinfo` in the guest should fail at once, not hang through
/// resolver retries against whatever the base image happened to carry. The
/// image's own CA bundle is untouched — no session certificate is minted or
/// installed.
///
/// # Errors
/// Returns a sentence naming whichever of those steps failed: the wire
/// could not be reached, `/etc/resolv.conf` could not be cleared, the
/// interface could not be planned or created, or the pump could not be
/// started.
fn bring_up_network(net_config: &Net) -> Result<Pid, String> {
    let socket = vsock::dial_host(net_config.port).map_err(|err| {
        format!(
            "could not reach the host's net wire on vsock port {}: {err}. The host must be \
             listening there before the guest boots, exactly as it must for the control plane.",
            net_config.port
        )
    })?;
    let wire = std::fs::File::from(socket);
    std::fs::write("/etc/resolv.conf", b"")
        .map_err(|err| format!("could not clear /etc/resolv.conf: {err}"))?;
    let tun = net::plan(net_config)?.apply()?;
    eprintln!(
        "ral-daemon: the network is up on vsock port {} as {}",
        net_config.port, net_config.address
    );
    let net_fd = OwnedFd::from(wire);
    let pump = pump::spawn(&tun, &net_fd)?;
    // As with the control plane above: the pump holds its own copies of
    // both descriptors now, and this process keeping either open would only
    // keep the tun and the net wire alive past the pump's own death.
    drop((tun, net_fd));
    eprintln!(
        "ral-daemon: net pump running as pid {}",
        pump.as_raw_nonzero()
    );
    Ok(pump)
}

/// Wait until there is a reason to stop, reaping everything that dies in the
/// meantime, and return that reason.
///
/// This is the whole of the daemon's running life.  Four things can end it:
/// the root engine dies, the pump dies (when this boot has one — see
/// `pump::epitaph` for why that ends the session exactly as the engine's
/// death does), a signal says the host wants the machine off, or — which
/// should not happen while the engine lives — the guest runs out of
/// processes entirely.
///
/// # Errors
/// Returns a sentence when taking a signal or reaping fails for a reason
/// other than an empty process table.
fn attend(root: Pid, pump: Option<Pid>, engines: &Engines) -> Result<String, String> {
    loop {
        let signal = reap::next_signal()
            .map_err(|err| format!("waiting for the guest's signals failed: {err}"))?;
        if signal != libc::SIGCHLD {
            return Ok(FACTORY_FAILURE
                .get()
                .cloned()
                .unwrap_or_else(|| "the host asked for this machine to stop".to_string()));
        }
        // Drained under the lock: a spawn in flight holds it, and std reaps
        // its own failed child before releasing it.  Torn down after it.
        let mut dead = Vec::new();
        let mut fleet = live(engines);
        loop {
            let waking = reap::poll_any()
                .map_err(|err| format!("reaping the guest's processes failed: {err}"))?;
            let (pid, death) = match waking {
                Waking::Reaped { pid, death } => (pid, death),
                Waking::Idle => break,
                Waking::Childless => {
                    return Ok(
                        "nothing is left running in the guest, not even the engine".to_string()
                    );
                }
            };
            if pid == root {
                return Ok(engine::epitaph(death));
            }
            if Some(pid) == pump {
                return Ok(pump::epitaph(death));
            }
            if fleet.forget(pid) {
                eprintln!("ral-daemon: engine pid {} {death}", pid.as_raw_nonzero());
                dead.push(pid);
            } else {
                // An orphan, reparented here when its own parent died.
                // Burying it is the job; saying so keeps the host's log
                // honest about what the guest was doing.
                eprintln!(
                    "ral-daemon: reaped orphan pid {}, which {death}",
                    pid.as_raw_nonzero()
                );
            }
        }
        drop(fleet);
        for pid in dead {
            let tree =
                Path::new(mounts::EXEC_CGROUP).join(format!("engine-{}", pid.as_raw_nonzero()));
            kill_cgroup(&tree);
            remove_cgroup(&tree, Instant::now() + SETTLE);
        }
    }
}

/// Kill every process in the cgroup at `path`, descendants included.  An
/// absent cgroup is an engine that never ran a command; any other failure is
/// logged, never fatal.
fn kill_cgroup(path: &Path) {
    match std::fs::write(path.join("cgroup.kill"), "1") {
        Err(err) if err.kind() != io::ErrorKind::NotFound => eprintln!(
            "ral-daemon: could not kill the commands under {}: {err}",
            path.display()
        ),
        _ => {}
    }
}

/// Remove the cgroup at `path` and every cgroup below it, deepest first.
/// `cgroup.kill` returns before its members die, so a busy cgroup is retried
/// until `deadline`; one left behind is logged and inert.
fn remove_cgroup(path: &Path, deadline: Instant) {
    for child in std::fs::read_dir(path).into_iter().flatten().flatten() {
        if child.file_type().is_ok_and(|kind| kind.is_dir()) {
            remove_cgroup(&child.path(), deadline);
        }
    }
    loop {
        match std::fs::remove_dir(path) {
            Err(err) if err.raw_os_error() == Some(libc::EBUSY) && Instant::now() < deadline => {
                std::thread::sleep(PULSE);
            }
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                return eprintln!("ral-daemon: could not remove {}: {err}", path.display());
            }
            _ => return,
        }
    }
}

/// The list, locked; a panicked pusher leaves it as consistent as before.
fn live(engines: &Engines) -> MutexGuard<'_, Fleet> {
    engines.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Bring the machine down, saying why first.
///
/// Every engine's process group is asked to finish, given [`GRACE`] for every
/// engine still alive to exit, and then killed; the commands an engine spawned
/// leave its group, so they are reached through the engine's cgroup instead:
/// the KILL sweep writes `cgroup.kill` at the root of all of them, which also
/// covers detached commands, whose tree is no engine's, and an engine already
/// dead. Only then is the pump
/// killed outright, with no grace at all. That order is deliberate, not
/// incidental: an engine that is finishing a job may still be draining a
/// response over the network, so killing the pump first would turn a graceful
/// five seconds into five seconds of hung sockets for no benefit — the pump
/// has no work of its own to finish, only the engines'. Then the filesystems
/// are flushed and the power goes off.
///
/// The root, when there is one, is the first of `engines`: `serve` pushes it
/// before the accept thread starts, and `attend` never forgets it.
///
/// # Errors
/// Returns a sentence if the kernel refuses to power the machine off, which
/// leaves the daemon with nothing further it can do.
fn halt(engines: &Engines, pump: Option<Pid>, why: &str) -> Result<Infallible, String> {
    eprintln!("ral-daemon: {why}");
    let all = live(engines).close();
    for &engine in &all {
        let _ = kill_process_group(engine, Signal::TERM);
    }
    // Wait only for engines still alive at TERM time: one already reaped (the
    // commonest way to arrive here is [`attend`] having just reaped the root)
    // is never reported again, so waiting on it could only expire. Each group
    // is swept either way: a process group outlives its leader for as long as
    // anything the engine spawned without leaving it is still in it.
    let mut pending: Vec<Pid> = all
        .iter()
        .copied()
        .filter(|&engine| rustix::process::test_kill_process(engine).is_ok())
        .collect();
    let deadline = Instant::now() + GRACE;
    while !pending.is_empty() && Instant::now() < deadline {
        match reap::poll_any() {
            Ok(Waking::Reaped { pid, .. }) => pending.retain(|&p| p != pid),
            Ok(Waking::Idle) => std::thread::sleep(PULSE),
            // Nobody left to wait for, or a wait that failed: either way
            // there is nothing more to learn here, and the SIGKILL below is
            // unconditional anyway.
            Ok(Waking::Childless) | Err(_) => break,
        }
    }
    for &engine in &all {
        let _ = kill_process_group(engine, Signal::KILL);
    }
    kill_cgroup(Path::new(mounts::EXEC_CGROUP));
    if let Some(pump) = pump {
        let _ = rustix::process::kill_process(pump, Signal::KILL);
    }
    // Everything the guest wrote to the workspace should reach the host
    // before the export carrying it — a virtiofs share or a 9p connection —
    // goes away with the machine.
    rustix::fs::sync();
    reboot(RebootCommand::PowerOff)
        .map_err(|err| format!("could not power the machine off: {err}"))?;
    Err("the kernel accepted the power-off and the machine is still running".to_string())
}

/// Say what the kernel actually handed us as `/`.
///
/// The stage before this one stacks a read-only rootfs image under a
/// per-session upper layer; whether it did is not something an init should
/// silently assume, and not something it should refuse to boot over either
/// — the engine still has its workspace, and refusing would be a policy
/// judgement this daemon does not make.  So it is reported, at second zero,
/// where a mis-assembled boot artifact is cheap to recognise.
fn announce_root() {
    match std::fs::read_to_string("/proc/mounts") {
        Err(err) => eprintln!("ral-daemon: could not read /proc/mounts: {err}"),
        Ok(table) => match mounts::root_filesystem(&table) {
            Some(root) if root.is_session_overlay() => {
                eprintln!("ral-daemon: / is the session overlay, writable as it should be");
            }
            Some(root) => eprintln!(
                "ral-daemon: / is {} and {}, not the session overlay the boot stage should have \
                 assembled; writes outside {} will fail or vanish with the machine",
                root.fstype,
                if root.writable {
                    "writable"
                } else {
                    "read-only"
                },
                mounts::WORK
            ),
            None => eprintln!("ral-daemon: /proc/mounts names no root filesystem"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(n: i32) -> Pid {
        Pid::from_raw(n).unwrap()
    }

    #[test]
    fn admission_closes_with_the_snapshot() {
        let mut fleet = Fleet::new(pid(2));
        assert_eq!(fleet.admit(|| Ok::<_, ()>(pid(3))), Some(Ok(pid(3))));
        assert_eq!(fleet.close(), [pid(2), pid(3)]);
        assert_eq!(fleet.admit(|| -> Result<Pid, ()> { unreachable!() }), None);
        assert_eq!(fleet.pids, [pid(2), pid(3)]);
    }

    #[test]
    fn forgetting_says_whether_it_was_an_engine() {
        let mut fleet = Fleet::new(pid(2));
        assert!(fleet.forget(pid(2)));
        assert!(!fleet.forget(pid(2)));
    }
}
