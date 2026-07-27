//! A human-driven smoke test for the net wire's prologue — the guest side
//! alone, with no `guest-net` stack behind the wire.
//!
//! `synod::session` exercises the whole path, but only with a provider
//! account and a conversation on top; this isolates the piece a boot-media
//! change is most likely to break — `ral-daemon` dialling the net wire,
//! reading a [`ral_daemon::packet::Prologue`], and writing what it carries
//! to `/etc/resolv.conf` and the CA store (`ral_daemon::net::delivery`) —
//! against a real boot rather than only the codec's own unit tests. It
//! boots one machine, plays the host stack's one relevant act (writing a
//! prologue), and leaves the rest to a person: watch `ral-daemon`'s console
//! lines above for `the network is up`, then use whatever this platform
//! offers to look inside the guest and confirm `/etc/resolv.conf` and
//! `printenv SSL_CERT_FILE` read back what was sent.
//!
//! See `boot-smoke.rs` for what each platform asks of you before this will
//! run at all — the same signing or group membership applies here.
//!
//! Usage: `net-prologue <kernel> <initramfs> <rootfs> <folder>`

use std::io::Write as _;

use ral_daemon::packet::Prologue;
use vm_manager::{BootArtifact, Hypervisor, MachineSpec};

#[cfg(unix)]
type NetStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type NetStream = std::net::TcpStream;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Ok([kernel, initramfs, rootfs, folder]) = <[String; 4]>::try_from(args) else {
        eprintln!("usage: net-prologue <kernel> <initramfs> <rootfs> <folder>");
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
    let wires = machine.take_wires();
    println!("booted; writing a prologue over the net wire...");

    let prologue = Prologue {
        resolv_conf: b"nameserver 10.0.2.2\n".to_vec(),
        ca_pem: b"-----BEGIN CERTIFICATE-----\nnet-prologue placeholder, not a real \
                  certificate\n-----END CERTIFICATE-----\n"
            .to_vec(),
    };
    let mut net = NetStream::from(wires.net);
    if let Err(err) = net.write_all(&prologue.encode()) {
        eprintln!("could not write the prologue: {err}");
        std::process::exit(1);
    }

    println!("prologue sent. Watch the console lines above for `the network is up`,");
    println!("then confirm inside the guest: cat /etc/resolv.conf; printenv SSL_CERT_FILE");
    println!("press enter to shut the machine down");
    let _ = std::io::stdin().read_line(&mut String::new());

    // Held open until here on purpose: the guest's pump ends the session the
    // moment this wire reports EOF, so closing it early would look like a
    // boot that came up and died on its own.
    drop(net);
    match machine.shutdown() {
        Ok(()) => println!("stopped cleanly"),
        Err(err) => {
            eprintln!("shutdown failed: {err}");
            std::process::exit(1);
        }
    }
}

/// The backend this platform has, constructed directly — see `boot-smoke.rs`
/// for why this bypasses `vm_manager::detect`.
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

/// See the macOS twin for why this is built directly rather than through
/// `vm_manager::detect`.
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

/// No backend here, and the example says so at run time rather than failing
/// to build — the same courtesy `vm_manager::detect` extends.
#[cfg(not(any(target_os = "macos", windows)))]
fn backend(artifact: BootArtifact) -> Option<Box<dyn Hypervisor>> {
    drop(artifact);
    None
}
