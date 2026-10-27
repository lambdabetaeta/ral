//! Root assembly precedes supervision; networking precedes every engine.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use guest_abi::boot::{Boot, Export, Net};
use guest_abi::layout;
use rustix::system::{RebootCommand, reboot};
use rustix::time::{ClockId, Timespec, clock_settime};

use super::fleet::{Engines, Fleet, make_engines};
use super::halt::{attend, halt};
use crate::{engine, mount, net, pump, reap, sysctl, vsock};

pub(crate) fn serve() -> Result<Infallible, String> {
    // Before any thread exists, so every thread inherits the block.
    reap::block()
        .map_err(|err| format!("could not block the signals ral-guest waits on: {err}"))?;

    // /proc precedes the configuration read that determines the remaining mounts.
    mount::procfs().apply()?;

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

    for filesystem in mount::plan(&boot) {
        filesystem.apply()?;
    }
    mount::scratch_temp()?;
    mount::exec_cgroup()?;
    let export = match &boot.workspace {
        Export::Virtiofs { tag } => format!("virtiofs tag `{tag}`"),
        Export::Plan9 { name, port } => {
            format!("the host's 9p share `{name}`, over vsock port {port}")
        }
    };
    eprintln!(
        "ral-guest: guest filesystems up; the granted folder is mounted at {} from {export}",
        layout::WORK
    );

    // Disable unprivileged user namespaces before engines to preserve cross-uid ptrace isolation.
    for setting in sysctl::PLAN {
        setting.apply()?;
    }

    // Ctrl-Alt-Del then reaches init as SIGINT, a stop request.
    if let Err(err) = reboot(RebootCommand::CadOff) {
        eprintln!("ral-guest: could not take Ctrl-Alt-Del's power to reboot the guest: {err}");
    }

    // Control dials first; the engine starts only once its network is ready.
    let control = engine::control_plane(boot.port)?;

    if let Some(net_config) = &boot.net {
        bring_up_network(net_config)?;
    }

    let listener = vsock::listen(guest_abi::ports::FACTORY).map_err(|err| {
        format!(
            "could not listen for the host's engine dials on vsock port {}: {err}. Does the \
             guest have a vsock device?",
            guest_abi::ports::FACTORY
        )
    })?;

    let root = engine::spawn(&boot, &control)?;
    // Close the parent's copy so it cannot keep the wire alive past the engine's death.
    drop(control);
    eprintln!(
        "ral-guest: engine running as pid {} on vsock port {}",
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

    halt(&engines, &attend(root, &engines)?)
}

/// Clear resolver state and bring networking up before any engine starts.
fn bring_up_network(net_config: &Net) -> Result<(), String> {
    let socket = vsock::dial_host(net_config.port()).map_err(|err| {
        format!(
            "could not reach the host's net wire on vsock port {}: {err}. The host must be \
             listening there before the guest boots, exactly as it must for the control plane.",
            net_config.port()
        )
    })?;
    std::fs::write("/etc/resolv.conf", b"")
        .map_err(|err| format!("could not clear /etc/resolv.conf: {err}"))?;
    let tun = net::plan(net_config).apply()?;
    eprintln!(
        "ral-guest: the network is up on vsock port {} as {}",
        net_config.port(),
        net_config.link().address
    );
    pump::start(tun, socket)?;
    eprintln!("ral-guest: net pump running as threads ral-net-out and ral-net-in");
    Ok(())
}

/// Report the root without refusing an unexpected filesystem.
fn announce_root() {
    match std::fs::read_to_string("/proc/mounts") {
        Err(err) => eprintln!("ral-guest: could not read /proc/mounts: {err}"),
        Ok(table) => match mount::root_filesystem(&table) {
            Some(root) if root.is_session_overlay() => {
                eprintln!("ral-guest: / is the session overlay, writable as it should be");
            }
            Some(root) => eprintln!(
                "ral-guest: / is {} and {}, not the session overlay the boot stage should have \
                 assembled; writes outside {} will fail or vanish with the machine",
                root.fstype,
                if root.writable {
                    "writable"
                } else {
                    "read-only"
                },
                layout::WORK
            ),
            None => eprintln!("ral-guest: /proc/mounts names no root filesystem"),
        },
    }
}
