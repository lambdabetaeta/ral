//! A human-driven smoke boot for real boot artifacts.
//!
//! Nothing in this crate's own tests boots a real kernel — a backend's
//! contract is exercised by construction, not by a machine actually coming
//! up — so there was no way to point a person at the real thing. This is
//! that pointer: it boots the three artifacts through whichever backend this
//! platform has and holds the machine open until asked to stop, so a person
//! can read `ral-daemon`'s own console lines and judge the boot honestly.
//!
//! # What each platform asks of you first
//!
//! - **macOS** — a signed binary, since Virtualization.framework refuses an
//!   unentitled one. Run `dev/scripts/sign-virtualization.sh
//!   target/debug/examples/boot-smoke` after every rebuild.
//! - **Windows** — an account the compute service serves: an administrator, or
//!   a member of the *Hyper-V Administrators* group. Nothing needs signing and
//!   the process itself is unprivileged; the group membership is what the
//!   service checks. Without it the boot refuses by naming the group, which is
//!   itself a useful thing to have seen.
//!
//! Usage: `boot-smoke <kernel> <initramfs> <rootfs> <folder>`

use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

use ral_core::io::TerminalState;
use ral_core::transport::{
    EnquiryError, Liveness, Program, Report, Run, TerminalEndpoint, Transport, WireTransport,
    dispatch_to_report,
};
use ral_core::types::Capabilities;
use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};
use vm_manager::{BootArtifact, Hypervisor, Machine, MachineSpec};

#[cfg(unix)]
type NetStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type NetStream = std::net::TcpStream;

/// The port the guest binds and the host dials. Any free number: it is chosen
/// by this example and known to both sides only because both are written here.
const PROBE_PORT: u32 = 1732;

/// The tag naming the boot recipe the guest engine applies once attached. The
/// guest's engine is the `exarch` binary (`vm-image/build-boot.sh`), whose
/// table registers this one tag (`exarch/src/shell_eval/builtins.rs`'s
/// `INSTALLER_TAG`). Spelled out rather than imported so this example needs no
/// dependency on `exarch`; a drift is loud, since the attach is refused by
/// name.
const INSTALLER_TAG: &str = "exarch-agent";

/// What the guest runs while the host dials it: bind the probe port, then
/// report every connection that arrives and where it came from.
///
/// It answers two questions at once. The host's dial proves the direction
/// carries bytes — the guest reads what the host wrote and writes back. The
/// two self-dials ask whether a *guest* process can reach the same port, which
/// is the only threat a token on this wire would defend against.
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
    let Some(hypervisor) = backend(artifact) else {
        eprintln!("this platform has no virtual-machine backend in vm-manager");
        std::process::exit(1);
    };

    println!("booting via {}...", hypervisor.name());
    let mut machine = match hypervisor.boot(&MachineSpec::for_folder(folder)) {
        Ok(machine) => machine,
        Err(err) => {
            eprintln!("boot failed: {err}");
            std::process::exit(1);
        }
    };
    let workspace = machine.workspace_path().to_path_buf();
    // Held open until the wire is dropped below, because the pump ends the
    // session on EOF — nothing needs to be sent on it first.
    let wires = machine.take_wires();
    let net = NetStream::from(wires.net);

    prove_the_host_can_dial_in(machine.as_ref(), wires.control, workspace);

    println!("booted: the agent can reach the granted folder and nothing else on this computer");
    println!("ral-daemon's own console lines print above as the guest runs.");
    println!("press enter to shut the machine down");
    let _ = std::io::stdin().read_line(&mut String::new());

    // Closing the wire first is what makes this the inside-out shutdown the
    // end of a real session performs: the pump sees EOF, the daemon powers
    // the machine off from within, and the wait below observes a stop rather
    // than forcing one.
    drop(net);
    match machine.shutdown() {
        Ok(()) => println!("stopped cleanly"),
        Err(err) => {
            eprintln!("shutdown failed: {err}");
            std::process::exit(1);
        }
    }
}

/// Put a listener inside the guest and dial it from the host, proving the
/// direction carries bytes rather than merely reporting a refusal.
///
/// The two halves must overlap: a dispatch is answered only when its run has
/// settled, so the dial has to happen while the guest's listener is still
/// accepting. The dialling thread is what makes them concurrent, and the
/// scope is what lets it borrow the machine.
fn prove_the_host_can_dial_in(
    machine: &dyn Machine,
    control: impl Into<ral_core::wire::WireStream>,
    workspace: std::path::PathBuf,
) {
    let transport = match WireTransport::adopt(control, Liveness::default()) {
        Ok(transport) => transport,
        Err(err) => {
            eprintln!("could not take control of the machine: {err}");
            return;
        }
    };
    transport.attach(
        TerminalEndpoint {
            lease: None,
            state: TerminalState::default(),
        },
        workspace,
        // The guest's own tmpfs, as every seat on this wire uses for `$HOME`.
        std::path::PathBuf::from("/tmp"),
        None,
        INSTALLER_TAG.to_string(),
    );

    let run = Run {
        program: Program::Source(format!("python3 << #####'{GUEST_LISTENER}'#####\n")),
        script_name: "<boot-smoke>".to_string(),
        caps: Capabilities::root(),
        wall: Some(Duration::from_secs(60)),
        deferred_lease: None,
        worker_cap: None,
        io: RunIo::Capture,
        terminal: RequestedTerminalAccess::Denied,
        stdin: RunStdin::Empty,
        trail: None,
    };

    println!("\nputting a vsock listener in the guest and dialling it from here...");
    // The listener runs on the spawned thread and the dial happens here, not
    // the other way round: `dyn Machine` is `Send` but not `Sync`, so the
    // machine stays on the thread that owns it.
    std::thread::scope(|scope| {
        let listening = scope.spawn(|| {
            dispatch_to_report(
                &transport,
                run,
                |surface| println!("guest surfaced: {surface:?}"),
                |_| Err(EnquiryError::no_desk()),
            )
        });

        // Long enough for the dispatch to reach the guest and python to bind;
        // the guest's accept window is wider than this by design.
        std::thread::sleep(Duration::from_secs(3));
        match dial_and_echo(machine) {
            Ok(echo) => println!("host read back: {echo:?}"),
            Err(err) => eprintln!("the dial failed: {err}"),
        }

        match listening.join() {
            Ok(Some(Report::Ran { captured, .. })) => {
                if let Some(captured) = captured {
                    print!("{}", String::from_utf8_lossy(&captured.stdout));
                    let stderr = String::from_utf8_lossy(&captured.stderr);
                    if !stderr.trim().is_empty() {
                        eprint!("guest stderr: {stderr}");
                    }
                }
            }
            Ok(Some(Report::Static { diagnostics })) => {
                eprintln!("the guest never ran the listener: {diagnostics:?}");
            }
            Ok(None) => eprintln!("the control plane closed without reporting"),
            Err(_) => eprintln!("the listening thread panicked"),
        }
    });

    transport.detach();
}

/// Dial the probe port, write a word, and read what the guest writes back.
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
        .write_all(b"hello-from-the-host")
        .map_err(|err| format!("could not write to the guest: {err}"))?;
    let mut echo = String::new();
    stream
        .read_to_string(&mut echo)
        .map_err(|err| format!("could not read the guest's echo: {err}"))?;
    Ok(echo)
}

/// The backend this platform has, constructed directly rather than through
/// [`vm_manager::detect`] — an example already holds the boot media `detect`
/// exists to look for, and asking it again would only add a way to be refused.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the `Option` is the seam's shape, not this arm's: a platform with a backend always \
              answers `Some`, and the one without answers `None` from the third arm below"
)]
#[cfg(target_os = "macos")]
fn backend(artifact: BootArtifact) -> Option<Box<dyn Hypervisor>> {
    Some(Box::new(vm_manager::vz::Vz::new(
        artifact,
        std::env::temp_dir(),
    )))
}

/// The backend this platform has — see the macOS twin for why it is built
/// directly. The cache is the real one synod would use rather than a temporary
/// directory: the rootfs wrap it may have to write is worth keeping between
/// runs.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the `Option` is the seam's shape, not this arm's: a platform with a backend always \
              answers `Some`, and the one without answers `None` from the third arm below"
)]
#[cfg(windows)]
fn backend(artifact: BootArtifact) -> Option<Box<dyn Hypervisor>> {
    use vm_manager::hcs::Hyperv;
    Some(Box::new(Hyperv::new(artifact, Hyperv::default_cache())))
}

/// No backend here, and the example says so at run time rather than failing to
/// build — the same courtesy [`vm_manager::detect`] extends.
#[cfg(not(any(target_os = "macos", windows)))]
fn backend(artifact: BootArtifact) -> Option<Box<dyn Hypervisor>> {
    drop(artifact);
    None
}
