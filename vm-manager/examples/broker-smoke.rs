//! A boot through the machine service, from an account with no privilege at
//! all.
//!
//! `boot-smoke` builds a [`Hyperv`](vm_manager::hcs::Hyperv) directly, which is
//! the maintainer's path and asks the maintainer's price: the compute service
//! serves only administrators and members of *Hyper-V Administrators*, so that
//! example must be run elevated. Every real user of synod is on the other path —
//! [`detect`](vm_manager::detect) finds the `LocalSystem` broker and asks *it*
//! for a machine — and that path had no smoke test, which is how a stale guest
//! image reached an installed synod and was met with sixty seconds of silence.
//! This is that test: the same boot the window performs, by the same route, as
//! whoever happens to be logged in.
//!
//! It takes one argument where `boot-smoke` takes four, and the difference is
//! the whole point. A brokered machine's kernel, initramfs and rootfs are the
//! ones installed beside the service and are never a caller's to name
//! ([`broker`](vm_manager::broker)'s own docs argue why), so the only thing left
//! to say is which folder the agent is being given.
//!
//! It boots and stops rather than holding the machine open: what is being
//! witnessed is that a guest comes up and dials, and a guest that has dialled
//! has already said everything this program can hear. When it does not, the
//! refusal now carries the guest's own last words off its console, which is the
//! other half of the same repair.
//!
//! Usage: `broker-smoke <folder>`

use vm_manager::MachineSpec;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Ok([folder]) = <[String; 1]>::try_from(args) else {
        eprintln!("usage: broker-smoke <folder>");
        std::process::exit(2);
    };

    // `None`, deliberately: this program holds no boot media and must not be
    // able to name any.  On Windows that is exactly what an installed synod
    // passes, and it is the service's own installed media that answers.  If no
    // service is listening, `detect` says so in the sentence it returns and
    // this example has nothing to add to it.
    let hypervisor = match vm_manager::detect(None) {
        Ok(hypervisor) => hypervisor,
        Err(why) => {
            eprintln!("no machine service could be asked for a guest: {why}");
            std::process::exit(1);
        }
    };

    println!(
        "asking {} for a machine over {folder}...",
        hypervisor.name()
    );
    let mut machine = match hypervisor.boot(&MachineSpec::for_folder(&folder)) {
        Ok(machine) => machine,
        Err(err) => {
            eprintln!("boot failed: {err}");
            std::process::exit(1);
        }
    };

    // Both wires are in hand only because the guest dialled both, in the
    // daemon's own order — so this line is the proof, and printing the mount
    // path with it says which folder the agent found at the far end.
    let wires = machine.take_wires();
    println!(
        "booted: the guest dialled the control plane and the net wire, and reached the granted \
         folder at {}",
        machine.workspace_path().display()
    );
    drop(wires);

    match machine.shutdown() {
        Ok(()) => println!("stopped cleanly, and its session disk went with it"),
        Err(err) => {
            eprintln!("shutdown failed: {err}");
            std::process::exit(1);
        }
    }
}
