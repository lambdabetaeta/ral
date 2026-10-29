//! The HCS document boots the same guest with SCSI disks, a 9p share and Hyper-V sockets.
//!
//! No network adapter is configured; the net wire is another socket service.
//! HCS handles have no thread affinity, so Guest owns its machine directly.

#![allow(
    clippy::disallowed_macros,
    reason = "[silent:windows-host] a host diagnostic with no ral shell to print through"
)]
use std::path::{Path, PathBuf};

use crate::error::{Act, Error, GuestConsole, HostFile, MachineFiles, Wire};
use crate::lifecycle::{
    Leaf, Missed, SessionDisk, aside_spelling, await_dials, stop_within, sweep, unix_seconds,
};
use crate::machine::{Booted, Hypervisor, Machine, Socket};
use crate::site::{Silent, site};
use crate::spec::{BootArtifact, MachineSpec, SIZE};

mod api;
mod console;
mod hvsock;
mod spec;
mod vhd;
pub mod win;

use win::{HYPERVISOR, plain};

use guest_abi::ports;

/// # Errors
/// Returns the permission refusal or a backend error naming the missing feature or service.
pub fn available() -> Result<(), Error> {
    let api = api::Api::open()?;
    api.probe_service().map_err(|error| {
        if error.code == api::HCS_E_ACCESS_DENIED {
            Error::NotPermitted
        } else {
            Error::backend(
                HYPERVISOR,
                format!("synod could not reach this computer's virtual-machine service: {error}"),
            )
        }
    })
}

pub struct Hyperv {
    artifact: BootArtifact,
    cache: PathBuf,
}

impl Hyperv {
    /// Construction sweeps orphaned session disks before starting a new session.
    pub fn new(artifact: BootArtifact, cache: impl Into<PathBuf>) -> Self {
        let cache = cache.into();
        sweep(&cache);
        Self { artifact, cache }
    }

    /// `LOCALAPPDATA` keeps the wrapped rootfs across reboots and temporary-directory sweeps.
    pub fn default_cache() -> PathBuf {
        site!(Silent::LocalAppDataVar, std::env::var_os("LOCALAPPDATA"))
            .map_or_else(
                || site!(Silent::LocalAppDataFallback, std::env::temp_dir()),
                PathBuf::from,
            )
            .join("Synod")
            .join("Machine")
    }
}

impl Hypervisor for Hyperv {
    /// # Errors
    /// Returns the folder, media, disk, document, compute-service or guest-dial refusal.
    fn boot(&self, spec: &MachineSpec) -> Result<Booted, Error> {
        // The compute service opens absolute, non-verbatim document paths in its own process.
        let folder = plain(spec.resolve()?);
        let resolved = self.artifact.resolve()?;
        let artifact = BootArtifact {
            kernel: plain(resolved.kernel),
            initramfs: plain(resolved.initramfs),
            rootfs: plain(resolved.rootfs),
        };
        let api = api::Api::open()?;

        let rootfs = vhd::ensure_rootfs_vhd(&artifact.rootfs, &self.cache)?;
        let session = SessionDisk::create(&self.cache, Leaf::Vhd, vhd::write_session_vhd)?;
        assemble(api, spec, &artifact, &folder, &rootfs, session, &self.cache)
    }
}

fn assemble(
    api: &'static api::Api,
    spec: &MachineSpec,
    artifact: &BootArtifact,
    folder: &Path,
    rootfs: &Path,
    session: SessionDisk,
    cache: &Path,
) -> Result<Booted, Error> {
    let machine_id = hvsock::fresh_machine_id();
    let id = hvsock::format_guid(&machine_id);

    // The console pipe must precede the machine that names it; losing diagnostics still permits boot.
    let console = match console::Console::create(&id, cache) {
        Ok(console) => Some(console),
        Err(why) => {
            eprintln!(
                "synod: the guest's console could not be opened: {}",
                why.worded(aside_spelling)
            );
            None
        }
    };

    let socket_sddl = hvsock::socket_sddl()?;
    let control_service = hvsock::format_guid(&hvsock::service_guid(ports::CONTROL));
    let net_service = hvsock::format_guid(&hvsock::service_guid(ports::NET));
    let document = spec::document(&spec::Plan {
        kernel: &artifact.kernel,
        initramfs: &artifact.initramfs,
        rootfs,
        session: session.path(),
        folder,
        read_only: spec.read_only,
        vcpus: clamp_cpus(SIZE.vcpus),
        memory_mib: SIZE.memory_mib,
        epoch: unix_seconds(),
        console_pipe: console.as_ref().map_or("", console::Console::pipe),
        socket_sddl: &socket_sddl,
        control_service: &control_service,
        net_service: &net_service,
    })?;
    let document = serde_json::to_string(&document).map_err(|e| {
        Error::backend(
            HYPERVISOR,
            format!("the machine's description could not be written: {e}"),
        )
    })?;

    let system = api.create(&id, &document).map_err(|error| {
        if error.code == api::HCS_E_ACCESS_DENIED {
            Error::NotPermitted
        } else {
            // HCS names no missing file, so report every file the document supplied.
            Error::NotConstructed {
                hypervisor: HYPERVISOR.into(),
                why: error.to_string(),
                files: Box::new(MachineFiles {
                    kernel: artifact.kernel.clone(),
                    initramfs: artifact.initramfs.clone(),
                    rootfs: rootfs.to_path_buf(),
                    session: session.path().to_path_buf(),
                    folder: folder.to_path_buf(),
                }),
            }
        }
    })?;
    // The machine exists, so from here every failure goes out through
    // `Guest`, whose `Drop` stops it and releases its disk.
    let session_path = session.path().to_path_buf();
    let live = Live {
        system,
        session,
        granted: Vec::new(),
    };
    let mut guest = Guest {
        api,
        id,
        live: Some(live),
        console,
        dialled: false,
    };

    if let Some(live) = &mut guest.live {
        // Grant boot files before start: the worker opens them as its own virtual account.
        for file in [&artifact.kernel, &artifact.initramfs, rootfs, &session_path] {
            let granted = api
                .grant_vm_access(&guest.id, file)
                .map_err(Error::host_file(HostFile::MachineFile, Act::Grant, file))?;
            if !granted {
                // The service decides whether files are accessible when the optional grant call is absent.
                eprintln!(
                    "synod: this Windows offers no HcsGrantVmAccess, so the machine's access to its \
                     own boot files is whatever the filesystem already allows"
                );
                break;
            }
            live.granted.push(file.to_path_buf());
        }
    }

    // Both listeners must be bound before the guest's first dial.
    let bind = |wire: Wire| {
        let port = match wire {
            Wire::Control => ports::CONTROL,
            Wire::Net => ports::NET,
        };
        hvsock::Listener::bind(&machine_id, port).map_err(|cause| {
            Error::backend(
                HYPERVISOR,
                format!(
                    "{} could not be opened for the machine: {cause}",
                    wire.noun()
                ),
            )
        })
    };
    let control_listener = bind(Wire::Control)?;
    let net_listener = bind(Wire::Net)?;

    api.start(system)
        .map_err(|error| Error::backend(HYPERVISOR, error.to_string()))?;

    let wires = await_dials(
        HYPERVISOR,
        |wire, patience| {
            let listener = match wire {
                Wire::Control => &control_listener,
                Wire::Net => &net_listener,
            };
            listener.accept_within(patience).map_err(|cause| {
                if cause.kind() == std::io::ErrorKind::TimedOut {
                    Missed::Timeout(cause.to_string())
                } else {
                    Missed::Failed(Error::backend(
                        HYPERVISOR,
                        format!("{} could not be accepted during boot: {cause}", wire.noun()),
                    ))
                }
            })
        },
        || guest.console.as_ref().map(kept),
    )?;
    // Both dials are in, so this boot has nothing left to explain and its
    // console log may go when the machine does.
    guest.dialled = true;
    Ok(Booted {
        machine: Box::new(guest),
        wires,
    })
}

fn kept(console: &console::Console) -> GuestConsole {
    GuestConsole::Kept {
        log: console.log().map(Path::to_path_buf),
        tail: console.tail(),
    }
}

pub(crate) struct Guest {
    api: &'static api::Api,
    id: String,
    live: Option<Live>,
    console: Option<console::Console>,
    /// Failed boots keep the log named in their refusal; only a dialled boot may discard it.
    dialled: bool,
}

struct Live {
    system: api::HcsSystem,
    session: SessionDisk,
    /// Access is revoked before the machine identity is closed.
    granted: Vec<PathBuf>,
}

// SAFETY: all fields are Send except the raw HCS handle, which has no thread affinity.
unsafe impl Send for Live {}

impl Guest {
    /// # Errors
    /// Returns `ForcedStop` if the guest outlives `STOP_GRACE`; repeated stops find released resources.
    fn stop(&mut self) -> Result<(), Error> {
        let Some(live) = self.live.take() else {
            return Ok(());
        };

        let outcome = stop_within(
            HYPERVISOR,
            || {},
            || {
                Ok(self.api.state(live.system).map_or(true, |state| {
                    state.is_none_or(|state| state.eq_ignore_ascii_case("stopped"))
                }))
            },
            || {
                let _ = self.api.shutdown(live.system);
                let _ = self.api.terminate(live.system);
            },
        );

        // Revoke access before closing the handle destroys the per-machine identity.
        for file in live.granted {
            self.api.revoke_vm_access(&self.id, &file);
        }
        self.api.close(live.system);
        // The pump may still be waiting for a machine that never connected.
        if let Some(console) = &self.console {
            console.wake();
            // See the `dialled` field: only a boot that succeeded is allowed to
            // take its console log with it.
            if self.dialled {
                console.discard();
            }
        }
        drop(live.session);
        outcome
    }
}

impl Machine for Guest {
    fn dial_engine(&self) -> std::io::Result<Socket> {
        self.connect_guest(ports::FACTORY)
    }

    fn connect_guest(&self, _port: u32) -> std::io::Result<Socket> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Hyper-V cannot yet dial a port inside its guest; this backend currently only accepts \
             connections opened by the guest",
        ))
    }

    fn console(&self) -> GuestConsole {
        self.console.as_ref().map_or_else(
            || GuestConsole::Kept {
                log: None,
                tail: Vec::new(),
            },
            kept,
        )
    }

    /// # Errors
    /// Returns [`Error::ForcedStop`] if the guest had to be stopped for.
    fn shutdown(mut self: Box<Self>) -> Result<(), Error> {
        self.stop()
    }
}

impl Drop for Guest {
    /// Dropping a machine stops it too, so a caller that forgets — or unwinds —
    /// does not leave a guest running.
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Hyper-V refuses a machine with more processors than the host.
fn clamp_cpus(vcpus: u8) -> u32 {
    let host = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    u32::from(vcpus).clamp(1, u32::try_from(host).unwrap_or(u32::MAX))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    #[test]
    fn boot_media_is_resolved_and_a_missing_file_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let make = |name: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, b"x").unwrap();
            path
        };
        let artifact = BootArtifact {
            kernel: make("kernel"),
            initramfs: make("initramfs.img"),
            rootfs: make("rootfs.img"),
        };
        let resolved = artifact.resolve().expect("every file is there");
        assert!(plain(resolved.kernel).is_absolute());

        let missing = BootArtifact {
            kernel: dir.path().join("nowhere"),
            ..artifact
        };
        match missing.resolve() {
            Err(Error::MissingBootFile { path, .. }) => assert!(path.ends_with("nowhere")),
            other => panic!("a missing kernel must be named: {other:?}"),
        }
    }

    #[test]
    fn processors_are_clamped_to_this_computer() {
        let host =
            u32::try_from(std::thread::available_parallelism().map_or(1, std::num::NonZero::get))
                .unwrap();
        assert_eq!(clamp_cpus(255), host);
        assert_eq!(clamp_cpus(1), 1);
        assert!((1..=host).contains(&clamp_cpus(4)));
    }

    #[test]
    fn the_cache_outlives_a_temp_sweep() {
        let cache = Hyperv::default_cache();
        assert!(
            cache.ends_with(Path::new("Synod").join("Machine")),
            "{cache:?}"
        );
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            assert!(cache.starts_with(PathBuf::from(local)));
        }
    }

    #[test]
    fn availability_answers_in_a_sentence() {
        match available() {
            Ok(()) => {}
            Err(refusal) => {
                let why = refusal.worded(|_| String::new()).to_string();
                assert!(
                    why.contains("Hyper-V Administrators")
                        || why.contains("Virtual Machine Platform")
                        || why.contains("virtual-machine service"),
                    "an unclassified refusal: {why}"
                );
            }
        }
    }
}
