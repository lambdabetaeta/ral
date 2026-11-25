//! Smoke boots require signed, entitled macOS binaries, or the Windows Hypervisor Platform
//! and a staged `openvmm.exe`.
//! macOS: `dev/scripts/sign-virtualization.sh target/debug/examples/boot-smoke`
//! Usage: `boot-smoke <kernel> <initramfs> <rootfs> <folder>`
//! Exits nonzero, after the machine is torn down, if any probe failed.

#![allow(
    clippy::disallowed_macros,
    reason = "[silent:example] an example prints its result"
)]
#![allow(clippy::disallowed_methods, reason = "example program")]

use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ral_core::carrier::{WireTransport, dispatch_to_report};
use ral_core::environ::EnvVars;
use ral_core::path::{EnginePath, GuestPath, shown};
use ral_core::protocol::channel::Liveness;
use ral_core::protocol::{Attach, EngineSpec, Program, Ran, Report, Root, Run};
use ral_core::shell::runtime::TitlePolicy;
use ral_core::source::SourceOrigin;
use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};
use vm_manager::{BootArtifact, Booted, DialError, Machine, MachineSpec};

#[cfg(unix)]
type NetStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type NetStream = std::net::TcpStream;

/// Chosen by this example; the guest listener and host dial must agree.
const PROBE_PORT: u32 = 1732;

/// No guest process listens here, so a dial must be refused.
const SILENT_PORT: u32 = 1733;

/// The guest listener answers it with `guest-echo:` and itself.
const GREETING: &str = "hello-from-the-host";

/// The guest registers this tag; spelling it here avoids an exarch dependency.
const INSTALLER_TAG: &str = "exarch-agent";

/// `exarch::app::SCRATCH_VAR`, spelled out for the same reason.
const SCRATCH_VAR: &str = "EXARCH_SCRATCH";

/// Guest self-dials exercise host-CID admission; the wire carries no token.
const GUEST_LISTENER: &str = r#"
import socket, struct, threading, time

PORT = 1732
WINDOW = 8.0

srv = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
srv.bind((socket.VMADDR_CID_ANY, PORT))
srv.listen(8)
srv.settimeout(0.5)
print("listening on vsock port %d" % PORT, flush=True)

def my_cid():
    import fcntl
    # IOCTL_VM_SOCKETS_GET_LOCAL_CID
    with open("/dev/vsock", "rb") as f:
        return struct.unpack("I", fcntl.ioctl(f, 0x7b9, b"\x00\x00\x00\x00"))[0]

try:
    mine = my_cid()
    print("my own cid is %d" % mine, flush=True)
except Exception as e:
    mine = None
    print("could not read my own cid: %r" % (e,), flush=True)

def self_dial(what, cid):
    s = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    s.settimeout(3)
    try:
        s.connect((cid, PORT))
        s.sendall(b"from-a-guest-process")
        s.close()
        print("self-dial via %s (cid %d): CONNECTED" % (what, cid), flush=True)
    except OSError as e:
        print("self-dial via %s (cid %d): refused, %s" % (what, cid, e), flush=True)

def probes():
    time.sleep(3)
    self_dial("VMADDR_CID_LOCAL", 1)
    if mine is not None:
        self_dial("my own cid", mine)

threading.Thread(target=probes, daemon=True).start()

seen_host = False
deadline = time.time() + WINDOW
while time.time() < deadline:
    try:
        conn, peer = srv.accept()
    except socket.timeout:
        continue
    cid = peer[0]
    data = conn.recv(64)
    conn.sendall(b"guest-echo:" + data)
    conn.close()
    if cid == socket.VMADDR_CID_HOST:
        seen_host = True
        who = "the HOST"
    else:
        who = "a guest process"
    print("accepted from %s (cid %d), read %r" % (who, cid, data), flush=True)

print("host dial: %s" % ("SEEN" if seen_host else "NOT SEEN"), flush=True)
"#;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Ok([kernel, initramfs, rootfs, folder]) = <[String; 4]>::try_from(args) else {
        eprintln!("usage: boot-smoke <kernel> <initramfs> <rootfs> <folder>");
        std::process::exit(2);
    };
    let artifact = BootArtifact {
        kernel: kernel.into(),
        initramfs: initramfs.into(),
        rootfs: rootfs.into(),
    };
    let hypervisor = match vm_manager::direct(artifact) {
        Ok(hypervisor) => hypervisor,
        Err(err) => {
            eprintln!(
                "could not construct a backend: {}",
                err.worded(|path| shown(path))
            );
            std::process::exit(1);
        }
    };

    println!("booting the machine...");
    let Booted { machine, wires } = match hypervisor.boot(&MachineSpec {
        folder: folder.into(),
        read_only: false,
    }) {
        Ok(booted) => booted,
        Err(err) => {
            eprintln!("boot failed: {}", err.worded(|path| shown(path)));
            std::process::exit(1);
        }
    };
    let workspace = GuestPath::from_static("/work");
    // Held open until the wire is dropped below, because the pump ends the
    // session on EOF — nothing needs to be sent on it first.
    let net = NetStream::from(wires.net);

    let mut failures = prove_the_host_can_dial_in(machine.as_ref(), wires.control, workspace);

    println!("booted: the agent can reach the granted folder and nothing else on this computer");
    println!("ral-guest's own console lines print above as the guest runs.");
    println!("press enter to shut the machine down");
    let _ = std::io::stdin().read_line(&mut String::new());

    // Closing the net wire lets the guest power off before shutdown waits.
    drop(net);
    match machine.shutdown() {
        Ok(()) => println!("stopped cleanly"),
        Err(err) => failures.push(format!(
            "shutdown failed: {}",
            err.worded(|path| shown(path))
        )),
    }
    if !failures.is_empty() {
        for failure in &failures {
            eprintln!("FAILED: {failure}");
        }
        std::process::exit(1);
    }
}

/// Dispatch and dial must overlap: dispatch returns only after the listener exits.
/// Returns every failed probe.
fn prove_the_host_can_dial_in(
    machine: &dyn Machine,
    control: impl Into<ral_core::protocol::channel::WireStream>,
    workspace: GuestPath,
) -> Vec<String> {
    let transport = match WireTransport::adopt(control, Liveness::default()) {
        Ok(transport) => transport,
        Err(err) => return vec![format!("could not take control of the machine: {err}")],
    };
    let scratch = GuestPath::from_static("/tmp");
    // The guest refuses an attach that does not name its scratch variable.
    let attach = Attach::root(
        EngineSpec::new(INSTALLER_TAG, EnginePath::Guest(scratch.clone())),
        Root::new(EnginePath::Guest(workspace))
            .with_env(EnvVars::from_pairs([(SCRATCH_VAR, scratch.to_string())]).unwrap()),
    );
    transport.attach(attach);
    if let Err(severed) = transport.await_attached() {
        return vec![format!("the guest refused to attach: {severed}")];
    }

    let run = Run {
        program: Program::Source(format!("python3 << #####'{GUEST_LISTENER}'#####\n")),
        origin: SourceOrigin::named("<boot-smoke>"),
        wall: Some(Duration::from_secs(60)),
        cleanup_grace: Duration::from_secs(5),
        worker_cap: None,
        worker_title: TitlePolicy::Optional,
        io: RunIo::Capture,
        terminal: RequestedTerminalAccess::Denied,
        stdin: RunStdin::Empty,
        trail: None,
        last: false,
    };

    println!("\nputting a vsock listener in the guest and dialling it from here...");
    // `dyn Machine` is Send but not Sync, so it stays here while the listener runs on another thread.
    std::thread::scope(|scope| {
        let listening = scope.spawn(|| dispatch_to_report(&transport, run, Arc::new(()), None));

        // The guest's accept window exceeds this delay for Python to bind.
        std::thread::sleep(Duration::from_secs(3));
        let expected = format!("guest-echo:{GREETING}");
        let echoed = match dial_and_echo(machine) {
            Ok(echo) if echo == expected => {
                println!("host read back: {echo:?}");
                Ok(())
            }
            Ok(echo) => Err(format!("the guest echoed {echo:?}, not {expected:?}")),
            Err(err) => Err(format!("the dial failed: {err}")),
        };
        let silent = match machine.connect_guest(SILENT_PORT) {
            Ok(_) => Err(format!(
                "port {SILENT_PORT} accepted a dial, though nothing listens there"
            )),
            Err(err) if unanswered(&err) => {
                println!("port {SILENT_PORT}, where nothing listens, refused: {err}");
                Ok(())
            }
            Err(err) => Err(format!(
                "port {SILENT_PORT}, where nothing listens, failed otherwise than unanswered: {err}"
            )),
        };
        let listened = match listening.join() {
            Ok(Ok(Report::Ran(Ran { captured, .. }))) => {
                if let Some(captured) = captured {
                    print!("{}", String::from_utf8_lossy(&captured.stdout.bytes));
                    let stderr = String::from_utf8_lossy(&captured.stderr.bytes);
                    if !stderr.trim().is_empty() {
                        eprint!("guest stderr: {stderr}");
                    }
                }
                Ok(())
            }
            Ok(Ok(Report::Checked { .. })) => {
                Err("the guest only checked the program; the listener never ran".to_string())
            }
            Ok(Ok(Report::Static { rendered, .. })) => {
                Err(format!("the guest never ran the listener: {rendered}"))
            }
            Ok(Ok(Report::Faulted { rendered, .. })) => Err(format!(
                "the guest faulted running the listener: {rendered}"
            )),
            Ok(Err(severed)) => Err(severed.to_string()),
            Err(_) => Err("the listening thread panicked".to_string()),
        };
        [echoed, silent, listened]
            .into_iter()
            .filter_map(Result::err)
            .collect()
    })
}

/// `OpenVMM`'s relay says exactly that nothing listened; elsewhere any refusal will do.
fn unanswered(err: &DialError) -> bool {
    cfg!(not(windows)) || matches!(err, DialError::Unanswered { port: SILENT_PORT })
}

fn dial_and_echo(machine: &dyn Machine) -> Result<String, String> {
    let started = Instant::now();
    let dial = machine
        .connect_guest(PROBE_PORT)
        .map_err(|err| format!("{err}"))?;
    println!("host dialled the guest in {:?}", started.elapsed());

    let mut stream = NetStream::from(dial);
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| format!("could not arm a read deadline: {err}"))?;
    stream
        .write_all(GREETING.as_bytes())
        .map_err(|err| format!("could not write to the guest: {err}"))?;
    let mut echo = String::new();
    stream
        .read_to_string(&mut echo)
        .map_err(|err| format!("could not read the guest's echo: {err}"))?;
    Ok(echo)
}
