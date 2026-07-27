//! The boot narrative itself: the one impure function, and the end it comes
//! to.
//!
//! Everything this module does is a syscall in a fixed order.  The decisions
//! it performs live in [`crate::boot`], [`crate::mounts`], [`crate::engine`],
//! [`crate::net`], [`crate::pump`], and [`crate::reap`], where they are data
//! and can be tested; here they are only carried out.

use std::convert::Infallible;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, getpid, kill_process_group};
use rustix::system::{RebootCommand, reboot};
use rustix::time::{ClockId, Timespec, clock_settime};

use crate::boot::{Boot, Export, Net};
use crate::reap::{self, Waking};
use crate::{engine, mounts, net, pump, sysctl, vsock};

/// How long the engine is given to finish after being asked to stop, before
/// it is killed.  Long enough for a run to abandon its work and flush the
/// workspace; short enough that a wedged guest does not hold a user's
/// afternoon.
const GRACE: Duration = Duration::from_secs(5);

/// How often the grace period looks to see whether the engine has gone.
const PULSE: Duration = Duration::from_millis(50);

/// Set by [`requested_stop`] the moment the host asks for this machine to
/// end.  The wait loop reads it whenever a signal interrupts it.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// The handler for every signal that means "the host wants this machine
/// off".  Async-signal-safe: one atomic store, nothing else.
extern "C" fn requested_stop(_: libc::c_int) {
    STOPPING.store(true, Ordering::Release);
}

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
    // Which of the two the host chose is worth one line in its own log: it is
    // the difference between the two hypervisors of §2, and the first thing
    // to know when the workspace misbehaves.
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

    listen_for_the_end();
    if STOPPING.load(Ordering::Acquire) {
        return halt(
            None,
            None,
            "the host asked to stop before the engine had started",
        );
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

    let engine = engine::spawn(&boot, &control)?;
    // The child holds its own copy on fd 3 now; the parent's would only keep
    // the connection alive past the engine's death.
    drop(control);
    eprintln!(
        "ral-daemon: engine running as pid {} on vsock port {}",
        engine.as_raw_nonzero(),
        boot.port
    );

    halt(Some(engine), pump, &attend(engine, pump)?)
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
/// the engine dies, the pump dies (when this boot has one — see
/// `pump::epitaph` for why that ends the session exactly as the engine's
/// death does), a signal says the host wants the machine off, or — which
/// should not happen while the engine lives — the guest runs out of
/// processes entirely.
///
/// # Errors
/// Returns a sentence when the wait itself fails for a reason that is
/// neither an interruption nor an empty process table.
fn attend(engine: Pid, pump: Option<Pid>) -> Result<String, String> {
    loop {
        let waking = reap::wait_any()
            .map_err(|err| format!("waiting for the guest's processes failed: {err}"))?;
        match waking {
            Waking::Reaped { pid, death } if pid == engine => return Ok(engine::epitaph(death)),
            Waking::Reaped { pid, death } if Some(pid) == pump => {
                return Ok(pump::epitaph(death));
            }
            // An orphan, reparented here when its own parent died. Burying
            // it is the job; saying so keeps the host's log honest about
            // what the guest was doing.
            Waking::Reaped { pid, death } => {
                eprintln!(
                    "ral-daemon: reaped orphan pid {}, which {death}",
                    pid.as_raw_nonzero()
                );
            }
            Waking::Interrupted if STOPPING.load(Ordering::Acquire) => {
                return Ok("the host asked for this machine to stop".to_string());
            }
            Waking::Interrupted => {}
            Waking::Childless => {
                return Ok("nothing is left running in the guest, not even the engine".to_string());
            }
            Waking::Idle => {
                unreachable!("a blocking wait is never idle; NOHANG belongs to reap::poll_any")
            }
        }
    }
}

/// Bring the machine down, saying why first.
///
/// The engine's whole session — it leads one, so `kill(-pid, …)` reaches
/// every command it ever spawned — is asked to finish, given [`GRACE`] to do
/// it, and then killed; only then is the pump killed outright, with no
/// grace at all. That order is deliberate, not incidental: an engine that is
/// finishing a job may still be draining a response over the network, so
/// killing the pump first would turn a graceful five seconds into five
/// seconds of hung sockets for no benefit — the pump has no work of its own
/// to finish, only the engine's. Then the filesystems are flushed and the
/// power goes off.
///
/// # Errors
/// Returns a sentence if the kernel refuses to power the machine off, which
/// leaves the daemon with nothing further it can do.
fn halt(engine: Option<Pid>, pump: Option<Pid>, why: &str) -> Result<Infallible, String> {
    eprintln!("ral-daemon: {why}");
    if let Some(engine) = engine {
        let _ = kill_process_group(engine, Signal::TERM);
        // Only an engine still running is worth waiting for.  The commonest
        // way to arrive here is [`attend`] having just reaped it, and a pid
        // already reaped is never reported a second time — so without this
        // probe the loop below could only ever expire, spending the whole
        // grace on a process that ended before it started.  The group is
        // swept either way: a process group outlives its leader for as long
        // as the commands the engine spawned are still in it.
        if rustix::process::test_kill_process(engine).is_ok() {
            let deadline = Instant::now() + GRACE;
            while Instant::now() < deadline {
                match reap::poll_any() {
                    Ok(Waking::Reaped { pid, .. }) if pid == engine => break,
                    Ok(Waking::Reaped { .. } | Waking::Idle | Waking::Interrupted) => {
                        std::thread::sleep(PULSE);
                    }
                    // Nobody left to wait for, or a wait that failed: either
                    // way there is nothing more to learn here, and the
                    // SIGKILL below is unconditional anyway.
                    Ok(Waking::Childless) | Err(_) => break,
                }
            }
        }
        let _ = kill_process_group(engine, Signal::KILL);
    }
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
/// §7 asks the stage before this one to stack a read-only rootfs image under
/// a per-session upper layer; whether it did is not something an init should
/// silently assume, and not something it should refuse to boot over either
/// — the engine still has its workspace, and refusing would be a policy
/// judgement this daemon does not make.  So it is reported, at second zero,
/// where a mis-assembled boot artifact is cheap to recognise.
fn announce_root() {
    match std::fs::read_to_string("/proc/mounts") {
        Err(err) => eprintln!("ral-daemon: could not read /proc/mounts: {err}"),
        Ok(table) => match mounts::root_filesystem(&table) {
            Some(root) if root.is_session_overlay() => {
                eprintln!("ral-daemon: / is the session overlay, writable as §7 expects");
            }
            Some(root) => eprintln!(
                "ral-daemon: / is {} and {}, not the session overlay §7 asks the boot stage to \
                 assemble; writes outside {} will fail or vanish with the machine",
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

/// Install the handlers for the end of the machine.
///
/// PID 1 is exempt from every default signal disposition — the kernel drops
/// signals init has not asked for — so a guest whose init installs nothing
/// simply cannot be stopped politely.  Three deliveries mean the same
/// request and are treated identically: `SIGTERM` from a supervisor,
/// `SIGPWR` from an ACPI power-button event, and `SIGINT`, which is what
/// Ctrl-Alt-Del becomes for init once [`RebootCommand::CadOff`] takes the
/// keystroke's power to reboot the machine out from under us.
fn listen_for_the_end() {
    // SAFETY: `requested_stop` is async-signal-safe (one atomic store) and
    // these three signals are claimed by nothing else in this process.
    unsafe {
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGPWR] {
            libc::signal(signal, requested_stop as *const () as libc::sighandler_t);
        }
    }
    if let Err(err) = reboot(RebootCommand::CadOff) {
        eprintln!("ral-daemon: could not take Ctrl-Alt-Del's power to reboot the guest: {err}");
    }
}
