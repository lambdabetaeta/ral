//! Hyper-V through the Host Compute System API — the Windows hardware backend.
//!
//! This is the Windows half of `dev/docs/VM/SYNOD.md` §2's two-backend design,
//! and it is deliberately the same *machine* as `vm-manager/src/vz.rs`'s, assembled out
//! of the parts Windows has instead of the parts macOS has:
//!
//! | The guest needs | macOS gives it | Windows gives it |
//! |---|---|---|
//! | direct kernel boot | `VZLinuxBootLoader` | `Chipset.LinuxKernelDirect` |
//! | two disks | virtio-blk, raw images | SCSI, VHD-wrapped images ([`vhd`]) |
//! | the granted folder | virtiofs share | 9p share the host serves ([`spec`]) |
//! | one socket out | `VZVirtioSocketDevice` | `AF_HYPERV` socket ([`hvsock`]) |
//! | a console | virtio console → stdout | COM port on a named pipe ([`console`]) |
//! | one net wire | second `VZVirtioSocketDevice` port | second `HvSocket` service ([`spec`]) |
//!
//! Neither backend configures a network adapter: the net wire is a second
//! socket, not a NIC, and the guest's TCP/IP stack lives entirely on the
//! host side of it (`dev/docs/VM/SYNOD.md` §6).
//!
//! The guest cannot tell the difference, and that is the point: the same
//! `ral-daemon` and the same engine boot under both, because every difference
//! above is either invisible from inside (the disk bus, the socket family) or
//! carried on the kernel command line (the console, the workspace transport).
//!
//! # What synod needs from Windows, and what it asks the user for
//!
//! Nothing here needs administrative rights at *run* time, but HCS itself is
//! gated: the compute service serves only administrators and members of the
//! **Hyper-V Administrators** group.  That is a deployment fact, not a bug —
//! §6 already has the university's IT department administering synod's policy —
//! and it is checked before a folder is granted rather than discovered halfway
//! into a session, which is what [`available`] is for.
//!
//! # Why there is no worker thread here
//!
//! `vm-manager/src/vz.rs` gives every machine a thread, because a `VZVirtualMachine` may
//! only be touched from the one serial queue it was born on.  An HCS compute
//! system has no such affinity: it is a handle, usable from any thread, with no
//! callback queue to host.  So a [`Guest`] holds its machine directly, and the
//! only threads in this backend serve blocking I/O — the console pump, and the
//! accept that waits for the guest to dial.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::{BootArtifact, Error, Hypervisor, Machine, MachineSpec, Wires};

mod api;
mod console;
mod hvsock;
mod spec;
mod vhd;

/// The `AF_VSOCK` port the guest dials for its control plane, and so the
/// service the host listens on ([`hvsock::service_guid`]).
///
/// The same number the macOS backend uses, and for the same reason: it is
/// written onto the kernel command line under `ral.port`, so host and guest
/// agree through the boot rather than through a shared constant.
const CONTROL_PORT: u32 = 1729;

/// How long [`Hyperv::boot`] waits for the guest to dial the control plane.
///
/// Twice the macOS backend's patience, because the work it spans is longer:
/// the compute service starts a virtual-machine worker process, the guest's
/// initramfs formats an eight-gigabyte session disk, and only then does the
/// daemon get to dial.  A cold `vmcompute` on a loaded laptop uses a good part
/// of this.
const BOOT_TIMEOUT: Duration = Duration::from_mins(1);

/// How long the guest is given to power itself off before the machine is
/// stopped for it.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// How often the stop path looks to see whether the machine has come to rest.
const STOP_PULSE: Duration = Duration::from_millis(50);

/// The backend's name, as a person should see it.
const HYPERVISOR: &str = "Hyper-V";

/// Shown when the compute service refuses this user.
///
/// The Windows analogue of macOS's missing-entitlement refusal, and the same
/// kind of thing: a machine that could host a guest, and a synod that is not
/// permitted to ask it to.  Windows explains the group membership itself
/// (`api`'s tests pin that), so this text says what to do about it in the
/// register a secretary reads, and leaves the mechanism to the detail.
const NOT_PERMITTED: &str = "synod is not allowed to create virtual machines on this computer — \
                             ask your IT department to add this account to the computer's \
                             'Hyper-V Administrators' group";

/// Shown when boot media is present and the platform could host a guest, but
/// the compute service is not answering at all.
fn service_unreachable(why: &str) -> String {
    format!("synod could not reach this computer's virtual-machine service: {why}")
}

/// Whether this Windows can host a guest for this user, and why not when it
/// cannot.
///
/// Asked by [`crate::detect`] before a session begins.  Three answers are
/// possible and each is a different remedy: the Virtual Machine Platform is not
/// installed (IT enables a Windows feature), this account may not use it (IT
/// adds a group membership), or the service is not running (a fault to report).
///
/// # Errors
/// Returns the sentence to show the person who granted the folder.
pub fn available() -> Result<(), String> {
    let api = api::Api::open()?;
    api.probe_service().map_err(|error| {
        if error.code == api::HCS_E_ACCESS_DENIED {
            NOT_PERMITTED.to_string()
        } else {
            service_unreachable(&error.to_string())
        }
    })
}

/// The Hyper-V backend.
///
/// Returned by [`crate::detect`] when this Windows can host a guest and the
/// application handed over real boot media; constructed explicitly by the
/// `boot-smoke` example, which already holds both.
pub struct Hyperv {
    artifact: BootArtifact,
    /// Where per-session disks are made and the wrapped rootfs is kept.
    cache: PathBuf,
}

impl Hyperv {
    /// A backend that boots `artifact`, keeping the disks it has to make or
    /// wrap under `cache`.
    pub fn new(artifact: BootArtifact, cache: impl Into<PathBuf>) -> Self {
        Self {
            artifact,
            cache: cache.into(),
        }
    }

    /// Where a machine's own files live: the wrapped rootfs, which outlives
    /// every session, and the session disks, which do not.
    ///
    /// `%LOCALAPPDATA%` rather than the temporary directory, so the two
    /// gigabytes [`vhd::ensure_rootfs_vhd`] may have to write survive a reboot
    /// and a temp sweep.  This duplicates a little of what synod's own
    /// directory layer knows, deliberately: this crate does not depend on
    /// `ral-core`, and a machine layer that reached into the application's
    /// bootstrap for a path would be the wrong way round.
    #[allow(
        clippy::disallowed_methods,
        reason = "REASONED-SILENT: lifting one path-valued environment variable. This is not an \
                  XDG basedir read — it is Windows' own cache location, and this crate \
                  deliberately does not depend on ral-core's basedir layer (see the doc above)."
    )]
    pub fn default_cache() -> PathBuf {
        std::env::var_os("LOCALAPPDATA")
            .map_or_else(std::env::temp_dir, PathBuf::from)
            .join("Synod")
            .join("Machine")
    }
}

impl Hypervisor for Hyperv {
    fn name(&self) -> &'static str {
        HYPERVISOR
    }

    /// Validate the spec, ready the disks, create the machine, listen for its
    /// guest, start it, and wait for the guest to dial before declaring it
    /// booted.
    ///
    /// The order is load-bearing at three points.  The console pipe exists
    /// before the machine that names it.  The control-plane listener is bound
    /// before the machine *starts*, because the guest's daemon dials with only
    /// a few seconds' patience and a listener bound afterwards would be racing
    /// a boot.  And the machine's access to its own boot files is granted
    /// before it is asked to open them — a virtual machine runs as its own
    /// virtual account, not as the user who created it.
    ///
    /// # Errors
    /// Returns [`Error`] if the spec is not usable (see
    /// [`MachineSpec::resolve`]), if a disk cannot be made or wrapped, if the
    /// compute service refuses the machine, or if the guest does not dial
    /// within [`BOOT_TIMEOUT`].
    fn boot(&self, spec: &MachineSpec) -> Result<Box<dyn Machine>, Error> {
        // Both of these resolve to absolute paths, and both are then spelled
        // plainly ([`plain`]) — every path in the document is opened by the
        // compute service, in its own process and its own directory, and read
        // out of JSON rather than passed to a Win32 call.
        let folder = plain(spec.resolve()?);
        let resolved = self.artifact.resolve()?;
        let artifact = BootArtifact {
            kernel: plain(resolved.kernel),
            initramfs: plain(resolved.initramfs),
            rootfs: plain(resolved.rootfs),
        };
        let api = api::Api::open().map_err(|why| unavailable(&why))?;

        let rootfs = vhd::ensure_rootfs_vhd(&artifact.rootfs, &self.cache)
            .map_err(|why| unavailable(&why))?;
        let session = vhd::create_session_vhd(&self.cache).map_err(|why| unavailable(&why))?;
        // From here on every early return must take the session disk with it:
        // it is this session's alone, and a machine that never started has no
        // teardown of its own to release it.
        let outcome = assemble(api, spec, &artifact, &folder, &rootfs, &session);
        if outcome.is_err() {
            let _ = remove(&session);
        }
        outcome
    }
}

/// Everything [`Hypervisor::boot`] does once the disks exist — split out so the
/// session disk has exactly one release path on failure.
///
/// A free function rather than a method, because by this point nothing of the
/// backend's own state is left to consult: the boot media has been resolved and
/// the disks made, so every path this needs arrives as an argument.
fn assemble(
    api: &'static api::Api,
    spec: &MachineSpec,
    artifact: &BootArtifact,
    folder: &Path,
    rootfs: &Path,
    session: &Path,
) -> Result<Box<dyn Machine>, Error> {
    let machine_id = hvsock::fresh_machine_id();
    let id = hvsock::format_guid(&machine_id);

    // The console first: the machine's document names this pipe, and the
    // service dials it when the machine starts.  A console is a diagnostic,
    // so failing to get one costs its output, not the session.
    let console = match console::Console::create(&id) {
        Ok(console) => Some(console),
        Err(why) => {
            eprintln!("synod: the guest's console could not be opened: {why}");
            None
        }
    };

    let socket_sddl = hvsock::socket_sddl().map_err(|why| unavailable(&why))?;
    let control_service = hvsock::format_guid(&hvsock::service_guid(CONTROL_PORT));
    let net_service = hvsock::format_guid(&hvsock::service_guid(crate::NET_PORT));
    let document = spec::document(&spec::Plan {
        kernel: &artifact.kernel,
        initramfs: &artifact.initramfs,
        rootfs,
        session,
        folder,
        read_only: spec.workspace.read_only,
        vcpus: clamp_cpus(spec.vcpus),
        memory_mib: spec.memory_mib,
        control_port: CONTROL_PORT,
        net_port: crate::NET_PORT,
        epoch: unix_seconds(),
        console_pipe: console.as_ref().map_or("", console::Console::pipe),
        socket_sddl: &socket_sddl,
        control_service: &control_service,
        net_service: &net_service,
    });
    let document = serde_json::to_string(&document).map_err(|e| {
        unavailable(&format!(
            "the machine's description could not be written: {e}"
        ))
    })?;

    let system = api.create(&id, &document).map_err(|error| {
        if error.code == api::HCS_E_ACCESS_DENIED {
            unavailable(NOT_PERMITTED)
        } else {
            // The service's own complaint about a file it could not open
            // names no file — "the system cannot find the path specified",
            // attributed only to `Construct` — so the five paths the
            // document carried are named here instead.  Every one of them
            // is opened by the service, in its own process, so every one is
            // a candidate and the reader should not have to guess.
            unavailable(&format!(
                "{error}. The machine's files were: kernel {}, initramfs {}, rootfs {}, \
                     session disk {}, and the granted folder {}",
                artifact.kernel.display(),
                artifact.initramfs.display(),
                rootfs.display(),
                session.display(),
                folder.display(),
            ))
        }
    })?;
    // The machine exists, so from here every failure goes out through
    // `Guest`, whose `Drop` stops it and releases its disk.
    let mut guest = Guest {
        api,
        id,
        system,
        mount_path: spec.workspace.guest_path.clone(),
        wires: None,
        console,
        session: session.to_path_buf(),
        granted: Vec::new(),
    };

    // A virtual machine reads its own boot files, as itself: without this the
    // service opens the kernel as `NT VIRTUAL MACHINE\<id>` and is refused by
    // the directory's own permissions.  Each granted file is remembered so
    // `Guest::stop` can take the access away again — the identity it names dies
    // with the machine, so an entry left behind is litter on someone's disk.
    for file in [&artifact.kernel, &artifact.initramfs, rootfs, session] {
        let granted = api
            .grant_vm_access(&guest.id, file)
            .map_err(|error| unavailable(&format!("{} — {error}", file.display())))?;
        if !granted {
            // The call does not exist on this Windows.  Say so once and carry
            // on: the machine may open its files anyway, and if it cannot, the
            // compute service's own complaint names the file.
            eprintln!(
                "synod: this Windows offers no HcsGrantVmAccess, so the machine's access to its \
                 own boot files is whatever the filesystem already allows"
            );
            break;
        }
        guest.granted.push(file.to_path_buf());
    }

    // Both listeners are bound before the machine starts, for the reason
    // [`hvsock::Listener::bind`] documents: the guest's daemon dials with a
    // few seconds' patience, and a listener bound afterwards would be racing
    // a boot.
    let control_listener = hvsock::Listener::bind(&machine_id, CONTROL_PORT).map_err(|cause| {
        unavailable(&format!(
            "the machine's control plane could not be opened: {cause}"
        ))
    })?;
    let net_listener = hvsock::Listener::bind(&machine_id, crate::NET_PORT).map_err(|cause| {
        unavailable(&format!(
            "the machine's net wire could not be opened: {cause}"
        ))
    })?;

    api.start(guest.system)
        .map_err(|error| unavailable(&error.to_string()))?;

    // A started machine is not a booted guest: a kernel can panic on the way
    // up and never reach userspace.  The two dials are the proof, taken in
    // the daemon's own order — control, then the net wire — and both are
    // charged against one deadline rather than two, so a guest that dials
    // the first promptly and never dials the second does not double the
    // patience a hung boot gets.
    let deadline = Instant::now() + BOOT_TIMEOUT;
    let control = control_listener
        .accept_within(deadline.saturating_duration_since(Instant::now()))
        .map_err(|cause| {
            unavailable(&format!(
                "the guest did not dial the control plane within {}s of starting: {cause}. The \
                 guest's own console output above says why, if it got far enough to say anything",
                BOOT_TIMEOUT.as_secs()
            ))
        })?;
    let net = net_listener
        .accept_within(deadline.saturating_duration_since(Instant::now()))
        .map_err(|cause| {
            unavailable(&format!(
                "the guest did not dial the net wire within {}s of starting: {cause}. The \
                 guest's own console output above says why, if it got far enough to say anything",
                BOOT_TIMEOUT.as_secs()
            ))
        })?;
    guest.wires = Some(Wires { control, net });
    Ok(Box::new(guest))
}

/// A booted guest, and the four things that have to be released when it stops:
/// its two wires, its machine, its console pump, and its session disk.
pub struct Guest {
    api: &'static api::Api,
    /// The machine's identifier — its `VmId`, and what `hcsdiag list` shows.
    id: String,
    system: api::HcsSystem,
    /// The path *inside* the guest where the granted folder is mounted.
    mount_path: PathBuf,
    /// The host ends of the control-plane and net-wire connections accepted
    /// at boot, taken out together by [`Machine::take_wires`].
    ///
    /// One `Option` around the pair, not two around each field: [`stop`]
    /// drops both wires in the one statement that clears this, which is what
    /// makes "drop both, or the pump never sees EOF on the one that
    /// lingered" true by construction rather than by remembering it.
    ///
    /// [`stop`]: Guest::stop
    wires: Option<Wires>,
    console: Option<console::Console>,
    session: PathBuf,
    /// The files this machine was given access to, to be taken away again when
    /// it stops.
    granted: Vec<PathBuf>,
}

// SAFETY: every field is `Send` but for `system`, a raw handle — and an HCS
// handle has no thread affinity at all (see the module docs), so moving one
// between threads is exactly as valid as using it from the thread that made it.
// `synod` moves a booted machine out of the scope that built it, which is the
// one thing this is needed for.
unsafe impl Send for Guest {}

impl Guest {
    /// Stop the machine and release what it holds.  Idempotent: the `Option`s
    /// and the null handle make a second call find nothing left to do, which is
    /// what lets [`Machine::shutdown`] and [`Drop`] share one path.
    ///
    /// # Errors
    /// Returns [`Error::Unavailable`] if the guest did not stop within
    /// [`STOP_GRACE`] and the machine had to be stopped for it.
    fn stop(&mut self) -> Result<(), Error> {
        // Closing the host ends of both wires first is what makes this a
        // *clean* shutdown rather than a kill: the guest's engine sees EOF on
        // the control plane, the daemon powers the machine off from inside,
        // and the grace window below observes a machine already stopping.
        // Dropping the one `Option` drops both sockets — see the field's own
        // doc for why that is not something a caller has to remember.  The
        // same inside-out shutdown the macOS backend performs.
        self.wires = None;
        if self.system.is_null() {
            return Ok(());
        }

        let forced = !self.stopped_within(STOP_GRACE) && {
            // The guest did not go on its own.  Ask the service to ask it —
            // which, with no ACPI helper in the guest, usually does nothing —
            // and then stop the machine outright rather than leave a session's
            // worth of memory running.
            let _ = self.api.shutdown(self.system);
            let _ = self.api.terminate(self.system);
            true
        };

        // The machine's identity dies with the handle, so its access entries are
        // taken away first — after this the entries would name nothing, and a
        // reader of the folder's permissions would be left puzzling over a SID
        // that resolves to no one.
        for file in std::mem::take(&mut self.granted) {
            self.api.revoke_vm_access(&self.id, &file);
        }
        self.api
            .close(std::mem::replace(&mut self.system, std::ptr::null_mut()));
        // The pump may still be waiting for a machine that never connected.
        if let Some(console) = &self.console {
            console.wake();
        }
        let _ = remove(&self.session);

        if forced {
            return Err(unavailable(
                "the guest did not shut down within the grace period, so the machine was stopped \
                 for it — anything it had not finished writing to the folder may be missing",
            ));
        }
        Ok(())
    }

    /// Whether the machine reaches `Stopped` within `patience`.
    ///
    /// Read from the service rather than assumed, because "the guest powered
    /// itself off" and "the guest hung" are the two outcomes
    /// [`Machine::shutdown`]'s contract has to tell apart.  A machine that has
    /// already gone answers with an error rather than a state, which is itself
    /// a stop.
    fn stopped_within(&self, patience: Duration) -> bool {
        let deadline = Instant::now() + patience;
        loop {
            match self.api.state(self.system) {
                Ok(None) | Err(_) => return true,
                Ok(Some(state)) if state.eq_ignore_ascii_case("stopped") => return true,
                Ok(Some(_)) => {}
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(STOP_PULSE);
        }
    }
}

impl Machine for Guest {
    /// The path *inside* the guest at which the granted folder is mounted.
    /// Never a host path: the host's own path for the folder is exactly what
    /// the agent must not be able to name.
    fn workspace_path(&self) -> &Path {
        &self.mount_path
    }

    /// The host ends of the control plane and the net wire the guest dialled
    /// at boot, handed over together, once.
    ///
    /// Both are `AF_HYPERV` sockets, where the macOS backend's are `AF_VSOCK`
    /// descriptors; both are adopted by `ral-core`'s wire the same way, and
    /// [`ral_core::wire::WireStream`](../../core/src/wire.rs) is where that is
    /// explained.
    ///
    /// # Panics
    /// Panics if asked a second time: a machine has exactly one of each wire.
    fn take_wires(&mut self) -> Wires {
        self.wires
            .take()
            .expect("a machine's two wires are taken at most once")
    }

    /// Close the wire, let the guest power itself off, and release the machine.
    ///
    /// # Errors
    /// Returns [`Error::Unavailable`] if the guest had to be stopped for.
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

/// A path as another process should be told it: absolute, and without the
/// `\\?\` prefix.
///
/// `std::fs::canonicalize` answers in Windows' *verbatim* form —
/// `\\?\C:\Users\…` — which is a convention of the Win32 call layer, where it
/// means "do not parse this, it is already final".  Every path in a machine's
/// document instead travels as text through JSON to a service that opens it by
/// its own means, and a verbatim prefix there is at best redundant and at worst
/// a path with four characters too many.  So it is stripped: `\\?\C:\x` becomes
/// `C:\x`, and a verbatim UNC path (`\\?\UNC\server\share`, which is what a
/// granted folder on a departmental file share canonicalises to —
/// `dev/docs/VM/SYNOD.md` §4 expects those) becomes `\\server\share` again.
/// Anything else is already plain and passes through untouched.
fn plain(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{unc}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path,
    }
}

/// An [`Error::Unavailable`] for this backend carrying `why`.
fn unavailable(why: &str) -> Error {
    Error::Unavailable {
        hypervisor: HYPERVISOR,
        why: why.to_string(),
    }
}

/// The spec's processor count, clamped to what this computer has.
///
/// Hyper-V refuses a machine with more processors than the host, so a spec
/// asking for four on a two-core laptop is a request to satisfy as far as it
/// can be, not a machine to refuse — the same reading `vm-manager/src/vz.rs` gives the
/// framework's own permitted range.
fn clamp_cpus(vcpus: u8) -> u32 {
    let host = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    u32::from(vcpus).clamp(1, u32::try_from(host).unwrap_or(u32::MAX))
}

/// The host's wall clock now, in whole seconds since the Unix epoch, for the
/// guest to adopt as its own (`ral.epoch`).
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Release a session disk, best-effort.
#[allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: releasing the per-session disk after power-off. Infrastructure \
              teardown, not a model's turn-time write; no Shell, no run, no card."
)]
fn remove(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: test scaffolding names literal paths and reads the cache location \
              back; no shell, no run, no card."
)]
mod tests {
    use super::*;

    /// Every path in a machine's document is stripped of the `\\?\` prefix
    /// `canonicalize` adds — including a file share's, which canonicalises to
    /// the verbatim UNC form and must go back to the `\\server\share` spelling
    /// every other reader of a path expects.  A plain path is left alone.
    #[test]
    fn a_documents_paths_are_spelled_plainly() {
        assert_eq!(
            plain(PathBuf::from(r"\\?\C:\synod\boot\kernel")),
            PathBuf::from(r"C:\synod\boot\kernel")
        );
        assert_eq!(
            plain(PathBuf::from(r"\\?\UNC\dept-files\secretaries\Invoices")),
            PathBuf::from(r"\\dept-files\secretaries\Invoices")
        );
        assert_eq!(
            plain(PathBuf::from(r"C:\already\plain")),
            PathBuf::from(r"C:\already\plain")
        );
    }

    /// Boot media is resolved to absolute paths before a document names it, and
    /// a file that is not there is refused by name rather than becoming the
    /// compute service's unattributed "cannot find the path specified".
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

    /// The backend names itself, for the greeting a user sees.
    #[test]
    fn the_backend_names_itself() {
        let hv = Hyperv::new(
            BootArtifact {
                kernel: PathBuf::from(r"C:\boot\kernel"),
                initramfs: PathBuf::from(r"C:\boot\initramfs.img"),
                rootfs: PathBuf::from(r"C:\boot\rootfs.img"),
            },
            r"C:\cache",
        );
        assert_eq!(hv.name(), HYPERVISOR);
    }

    /// A machine never gets more processors than the computer has, and never
    /// fewer than one — a spec is a request, and the host is the ceiling.
    #[test]
    fn processors_are_clamped_to_this_computer() {
        let host =
            u32::try_from(std::thread::available_parallelism().map_or(1, std::num::NonZero::get))
                .unwrap();
        assert_eq!(clamp_cpus(255), host);
        assert_eq!(clamp_cpus(1), 1);
        assert!((1..=host).contains(&clamp_cpus(4)));
    }

    /// The cache is a place that survives a reboot, not the temporary
    /// directory — the wrapped rootfs is two gigabytes and should be written
    /// once, not once per sweep.
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

    /// Whatever this computer answers about hosting a guest, it answers in a
    /// sentence a secretary could act on — and the access denial names the
    /// group, since that is the one an IT department fixes.
    #[test]
    fn availability_answers_in_a_sentence() {
        match available() {
            Ok(()) => {}
            Err(why) => {
                assert!(!why.is_empty());
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
