//! A human-driven proof that a run crosses the wire into a booted guest.
//!
//! Where `vm-manager`'s `boot-smoke` example ends at "the engine is alive on
//! the guest's socket", this one drives the §3 protocol over that same wire: it
//! boots the three artifacts through whichever backend this platform has,
//! adopts the control plane `take_wires` hands back into a [`WireTransport`],
//! attaches a session at the guest's `/work`, and dispatches one real run —
//! reading the granted folder from inside the VM — to a settled [`Report`]. The
//! captured output comes back across the wire, proving the whole path end to
//! end: boot, workspace share, engine, and the frame protocol under a running
//! run.
//!
//! It is the same program on both platforms, and that is the interesting part.
//! Nothing below is `#[cfg]`-ed except which backend is constructed: the
//! control plane is an `AF_VSOCK` descriptor under Virtualization.framework and
//! an `AF_HYPERV` socket under Hyper-V, and
//! [`WireTransport::adopt`](ral_core::transport::WireTransport::adopt) takes
//! either, so the protocol never learns which hypervisor it is talking through.
//!
//! # What each platform asks of you first
//!
//! - **macOS** — a signed binary. Run `dev/scripts/sign-virtualization.sh
//!   target/debug/examples/boot-run` after every rebuild.
//! - **Windows** — an account the compute service serves: an administrator, or a
//!   member of the *Hyper-V Administrators* group.
//!
//! Usage: `boot-run <kernel> <initramfs> <rootfs> <folder>`

use std::io::Write as _;

use ral_core::io::TerminalState;
use ral_core::transport::{
    EnquiryError, Liveness, Program, Report, Run, TerminalEndpoint, Transport, WireTransport,
    dispatch_to_report,
};
use ral_core::types::Capabilities;
use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};
use ral_daemon::packet::Prologue;
use vm_manager::{BootArtifact, Hypervisor, MachineSpec};

#[cfg(unix)]
type NetStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type NetStream = std::net::TcpStream;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Ok([kernel, initramfs, rootfs, folder]) = <[String; 4]>::try_from(args) else {
        eprintln!("usage: boot-run <kernel> <initramfs> <rootfs> <folder>");
        std::process::exit(2);
    };

    // Seed a file the guest run will read back, so the captured output is
    // proof the granted folder crossed into the guest — over virtiofs on one
    // platform, over the host's own 9p server on the other — and not merely
    // that a run ran.
    let sentinel = "boot-run-was-here";
    #[allow(
        clippy::disallowed_methods,
        reason = "REASONED-SILENT: example scaffolding seeds the folder it then grants; no shell, no run, no card"
    )]
    std::fs::write(format!("{folder}/sentinel.txt"), sentinel).expect("seed the granted folder");

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
    println!("booted: the agent can reach the granted folder and nothing else on this computer");

    let workspace = machine.workspace_path().to_path_buf();
    // Both wires arrive at boot, and the guest waits on the net wire's
    // prologue before it will start an engine at all — so a host that takes
    // that wire and says nothing wedges the boot rather than merely going
    // without a network. This example wants no network, only an engine, so it
    // sends the shortest honest prologue there is (no resolver, no proxy to
    // trust) and then holds the wire open: the pump ends the session the
    // moment it reports EOF.
    let wires = machine.take_wires();
    let mut net = NetStream::from(wires.net);
    net.write_all(
        &Prologue {
            resolv_conf: Vec::new(),
            ca_pem: Vec::new(),
        }
        .encode(),
    )
    .expect("write the net wire's prologue");

    let transport = WireTransport::adopt(wires.control, Liveness::default())
        .expect("adopt the guest's control plane");

    transport.attach(
        TerminalEndpoint {
            lease: None,
            state: TerminalState::default(),
        },
        workspace.clone(),
        workspace.clone(),
        None,
        exarch::shell_eval::builtins::INSTALLER_TAG.to_string(),
    );

    let src = format!("cat {}/sentinel.txt", workspace.display());
    let report = dispatch_to_report(
        &transport,
        Run {
            program: Program::Source(src),
            script_name: "<boot-run>".into(),
            caps: Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Capture,
            terminal: RequestedTerminalAccess::Denied,
            stdin: RunStdin::Empty,
        },
        |_| {},
        |_| {},
        |_| -> Result<_, EnquiryError> { unreachable!("this run raises no enquiry") },
    )
    .expect("the guest engine must answer the dispatch with a Report");

    let passed = matches!(
        &report,
        Report::Ran { result: Ok(_), captured: Some(c), .. }
            if String::from_utf8_lossy(&c.stdout).contains(sentinel)
    );
    if passed
        && let Report::Ran {
            captured: Some(c), ..
        } = &report
    {
        println!("run ran in the guest; it read the granted folder back over the wire:");
        println!("  {}", String::from_utf8_lossy(&c.stdout).trim());
        println!("PASS: a run crossed the wire and saw the workspace");
    }
    if !passed {
        eprintln!("FAIL: unexpected report {report:?}");
    }

    // Close the wire before powering off, exactly as `session.rs` does when
    // the window ends the session: the engine sees EOF on its control socket,
    // exits, and ral-daemon — whose whole running life waits for the engine
    // to die — powers the machine off from inside. That inside-out poweroff
    // is the real shutdown path; holding the wire open here would leave only
    // the ACPI request a minimal guest has nothing to answer.
    drop(transport);

    match machine.shutdown() {
        Ok(()) => println!("stopped cleanly"),
        Err(err) => eprintln!("shutdown failed: {err}"),
    }
    if !passed {
        std::process::exit(1);
    }
}

/// The backend this platform has, constructed directly rather than through
/// `vm_manager::detect` — an example already holds the boot media, and
/// `synod::boot` is what finds it in a shipped build.
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

/// The backend this platform has — see the macOS twin.
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
/// build.
#[cfg(not(any(target_os = "macos", windows)))]
fn backend(artifact: BootArtifact) -> Option<Box<dyn Hypervisor>> {
    drop(artifact);
    None
}
