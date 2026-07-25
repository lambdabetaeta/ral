//! A human-driven proof that a run crosses the wire into a booted guest.
//!
//! Where `vm-manager`'s `boot-smoke` example ends at "the engine is alive on
//! the vsock", this one drives the §3 protocol over that same wire: it boots
//! the three artifacts through the `vz` backend, adopts the control plane
//! `take_control` hands back into a [`WireTransport`], attaches a session at
//! the guest's `/work`, and dispatches one real run — reading the granted
//! folder from inside the VM — to a settled [`Report`]. The captured output
//! comes back across the wire, proving the whole path end to end: boot,
//! workspace share, engine, and the frame protocol under a running run.
//!
//! Requires a signed binary — Virtualization.framework refuses an unentitled
//! one. Run `dev/scripts/sign-virtualization.sh target/debug/examples/boot-run`
//! after every rebuild.
//!
//! Usage: `boot-run <kernel> <initramfs> <rootfs> <folder>`

#[cfg(target_os = "macos")]
fn main() {
    use std::os::unix::net::UnixStream;

    use ral_core::io::TerminalState;
    use ral_core::transport::{
        EnquiryError, Liveness, Program, Report, TerminalEndpoint, Transport, Run, WireTransport,
        dispatch_to_report,
    };
    use ral_core::types::Capabilities;
    use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};
    use vm_manager::vz::Vz;
    use vm_manager::{BootArtifact, Hypervisor, MachineSpec};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Ok([kernel, initramfs, rootfs, folder]) = <[String; 4]>::try_from(args) else {
        eprintln!("usage: boot-run <kernel> <initramfs> <rootfs> <folder>");
        std::process::exit(2);
    };

    // Seed a file the guest run will read back, so the captured output is
    // proof the granted folder crossed the virtiofs share, not just that a
    // run ran.
    let sentinel = "boot-run-was-here";
    #[allow(
        clippy::disallowed_methods,
        reason = "REASONED-SILENT: example scaffolding seeds the folder it then grants; no shell, no run, no card"
    )]
    std::fs::write(format!("{folder}/sentinel.txt"), sentinel).expect("seed the granted folder");

    let vz = Vz::new(
        BootArtifact {
            kernel: kernel.into(),
            initramfs: initramfs.into(),
            rootfs: rootfs.into(),
        },
        std::env::temp_dir(),
    );

    println!("booting via {}...", vz.name());
    let mut machine = match vz.boot(&MachineSpec::for_folder(folder)) {
        Ok(machine) => machine,
        Err(err) => {
            eprintln!("boot failed: {err}");
            std::process::exit(1);
        }
    };
    println!("booted: the agent can reach the granted folder and nothing else on this computer");

    let workspace = machine.workspace_path().to_path_buf();
    let control = machine.take_control();
    let transport = WireTransport::adopt(UnixStream::from(control), Liveness::default())
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

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("boot-run drives vm_manager::vz, which builds on macOS only");
    std::process::exit(1);
}
