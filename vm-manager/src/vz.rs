//! Apple's Virtualization.framework — the macOS hardware backend.
//!
//! This module was, until it was first compiled on an Apple Silicon Mac, a
//! seam: every body was `todo!` and the doc comments carried the contract.
//! It is now real code.  It builds a `VZVirtualMachineConfiguration` from a
//! [`MachineSpec`] and a [`BootArtifact`], validates it, and — on a signed,
//! entitled binary with a real boot image — starts the guest and holds it
//! open behind the trait the rest of the crate speaks.
//!
//! # The binding
//!
//! Virtualization.framework is an Objective-C API, so a Rust backend needs a
//! binding.  The two candidates — a Swift shim and the `vz` crate — were to
//! be decided between by trying.  Both were tried on paper and one survives:
//!
//! - **`objc2-virtualization`** (chosen) — part of the maintained `objc2`
//!   framework-crate family, generated from the SDK headers.  It exposes every
//!   class this backend names — `VZLinuxBootLoader`,
//!   `VZVirtioBlockDeviceConfiguration`,
//!   `VZVirtioFileSystemDeviceConfiguration`, `VZVirtioSocketDevice`, the
//!   listener delegate protocol, the serial-console attachment — behind a
//!   default feature set, and it drives the machine from Rust with no foreign
//!   toolchain: no `swiftc`, no build script, no `.swift` file to keep in
//!   sync with this one.  The whole backend compiles under plain `cargo`.
//! - **The `vz` crate** (rejected) — the name the design record wrote down no
//!   longer denotes a Virtualization.framework wrapper at all: on crates.io
//!   today `vz` is an unrelated file-encryption CLI.  There is no
//!   Virtualization binding by that name to link, so the choice was between
//!   `objc2-virtualization` and hand-writing a Swift shim.  A shim buys
//!   nothing here — it would add a second language, a second build step, and
//!   an FFI boundary to marshal the same objects across — so it loses to the
//!   in-language binding on every axis that matters.
//!
//! # What the configuration carries
//!
//! `build_configuration` assembles a `VZVirtualMachineConfiguration` with:
//!
//! - a `VZLinuxBootLoader` for direct kernel boot — the kernel and initramfs
//!   from the [`BootArtifact`], with a command line that names the console
//!   (`console=hvc0`, the virtio serial port below) and the guest-side session
//!   settings `ral-daemon` reads: the virtiofs `WORKSPACE_TAG`, the
//!   control-plane `CONTROL_PORT`, and the host's wall clock;
//! - two `VZVirtioBlockDeviceConfiguration`s — the read-only rootfs image
//!   first (the guest's `/dev/vda`) and the read-write session image second
//!   (`/dev/vdb`), which the guest's initramfs stacks into the overlay root;
//! - a `VZVirtioFileSystemDeviceConfiguration` sharing the granted folder
//!   under `WORKSPACE_TAG`, built from a `VZSingleDirectoryShare` over a
//!   `VZSharedDirectory` carrying the spec's `read_only` flag, so read-only
//!   is the mount's law and not a matter of policy;
//! - a `VZVirtioSocketDevice` multiplexing both wires — `CONTROL_PORT` and
//!   `NET_PORT` — and **no network device at all**: the guest's only way off
//!   the machine is that one socket, port-routed;
//! - a `VZVirtioConsoleDeviceSerialPortConfiguration` writing the guest's
//!   console to the host's standard output, which is where the daemon's log
//!   lines surface;
//! - `CPUCount` and `memorySize` from the spec, each clamped into the range
//!   `VZVirtualMachineConfiguration` permits.
//!
//! `validateWithError:` is called before the configuration is ever handed to
//! a `VZVirtualMachine`, and macOS's complaint is carried into
//! [`Error::Unavailable`] verbatim — it is the one message that says which
//! part of the configuration the framework refused.
//!
//! # The run loop, and why the machine lives behind a thread
//!
//! `VZVirtualMachine` is not `Send`, and — the sharper constraint — every one
//! of its methods must run on the one serial dispatch queue it was created
//! with, where its completion handlers and delegate callbacks are also
//! delivered.  A library function like [`Vz::boot`] cannot commandeer the
//! process's main thread or run loop to be that queue's home, so the machine
//! gets a thread of its own: [`Vz::boot`] spawns a worker that builds the
//! configuration, creates the `VZVirtualMachine` against a private serial
//! queue, and from then on is the *only* code that touches it.  The
//! [`Guest`] the caller holds owns none of that; it owns a channel to the
//! worker and speaks to the machine by message.  Operations reach the queue
//! as blocks run with `dispatch_sync`, which is what lets a `!Send` machine be
//! driven correctly from Rust: the block borrows the machine for the length
//! of a synchronous call and never carries it across a thread.
//!
//! Boot is not declared done when `start` returns — a kernel can panic on the
//! way up and never reach userspace.  The worker installs a
//! `VZVirtioSocketListener` on `CONTROL_PORT` *and* one on `NET_PORT` before
//! starting, and waits for the guest's daemon to dial the host on both
//! (`ral-daemon`'s `control_plane` and its net-wire counterpart).  Two
//! connections are proof the guest booted far enough to run its init and open
//! its net wire; only then does [`Vz::boot`] return.  Neither accepted
//! connection is spent on the readiness signal and discarded — each *is* a
//! stream a later stage speaks over (the control-plane frame protocol, and
//! the net wire's packet stream), so each host end is duplicated out of the
//! framework's connection object and carried back to the caller on the
//! [`Guest`]'s [`Wires`](crate::Wires), to outlive the object VZ may close.
//!
//! # Entitlement and signing — what fails without them, and how
//!
//! *Building* a configuration — creating the objects, wiring the disk stack,
//! the share, the socket, the console — needs nothing special, and the unit
//! tests below do exactly that under a plain `cargo test` binary.  But the
//! entitlement wall stands earlier than one might guess: `validateWithError:`
//! *itself* is refused in an unentitled process, with the message *"Invalid
//! virtual machine configuration. The process doesn't have the
//! com.apple.security.virtualization entitlement."*  So even validation — let
//! alone starting a machine — cannot succeed here; the most a test can prove
//! is that a well-formed configuration is turned away for the entitlement and
//! nothing else.  Virtualization.framework is gated by
//! `com.apple.security.virtualization`, which the kernel grants only to a
//! code-signed binary that carries it.  A bare `cargo` test or debug binary
//! carries no such entitlement — which is exactly why [`crate::detect`]
//! answers with this backend only after [`entitled`] says the process holds
//! the grant, and refuses outright otherwise.
//!
//! In the shipped product the entitlement rides on the synod application
//! bundle: the `.app` is signed with a provisioning profile (or, for local
//! development, an ad-hoc signature plus a `.entitlements` plist granting
//! `com.apple.security.virtualization`), and the multicall binary inside it
//! inherits the grant.  Without it, `VZVirtualMachine` creation and `start`
//! are refused by the framework; the refusal surfaces as a `VZErrorDomain`
//! error carried into [`Error::Unavailable`], and on stricter macOS releases
//! as an abort at machine-creation time.  That failure path cannot be
//! exercised here — this Mac's test binary is unentitled by construction — so
//! it is documented rather than tested; the live-machine path is exercised by
//! the signed `boot-smoke` and `boot-run` examples, never by `cargo test`.

use std::ffi::c_void;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use block2::{DynBlock, RcBlock};
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::NSArray;
use objc2_foundation::{NSFileHandle, NSNumber, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_virtualization::{
    VZDiskImageStorageDeviceAttachment, VZFileHandleSerialPortAttachment, VZLinuxBootLoader,
    VZSharedDirectory, VZSingleDirectoryShare, VZStorageDeviceConfiguration,
    VZVirtioBlockDeviceConfiguration, VZVirtioConsoleDeviceSerialPortConfiguration,
    VZVirtioFileSystemDeviceConfiguration, VZVirtioSocketConnection, VZVirtioSocketDevice,
    VZVirtioSocketDeviceConfiguration, VZVirtioSocketListener, VZVirtioSocketListenerDelegate,
    VZVirtualMachine, VZVirtualMachineConfiguration, VZVirtualMachineState,
};

use crate::{BootArtifact, Error, Hypervisor, Machine, MachineSpec};

/// The virtiofs tag under which the granted folder is exported to the guest.
///
/// The host and guest agree on the granted folder only through this word: it
/// is written into the kernel command line under `ral.workspace` and read
/// back by the daemon, which mounts it at [`MachineSpec::GUEST_WORKSPACE`].
/// It is a name, not a path, short enough for virtiofs's own tag limit.
const WORKSPACE_TAG: &str = "workspace";

/// The `AF_VSOCK` port the host listens on for the guest's control-plane
/// connection.  The guest dials `VMADDR_CID_HOST` on this port; the number is
/// carried to it on the kernel command line under `ral.port`.
const CONTROL_PORT: u32 = 1729;

/// The size of the per-session read-write disk, in bytes (8 GiB, sparse).
///
/// The image is created empty and grows on demand; the guest's initramfs
/// formats it and uses it as the overlay upper layer plus scratch.
const SESSION_IMAGE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// How long [`Vz::boot`] waits for the guest to dial the control plane before
/// giving up.  Generous, because it spans a kernel boot and a userland init.
const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a stop request is given to bring the machine to `Stopped` on its
/// own before it is forced.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// How often the stop path looks to see whether the machine has come to rest.
const STOP_PULSE: Duration = Duration::from_millis(50);

/// The Virtualization.framework backend.
///
/// Returned by [`detect`](crate::detect) when this process is [`entitled`]
/// and the application handed over real boot media; constructed explicitly
/// by callers — the signed examples — that already hold the boot artifact
/// and a directory to keep session images in.
pub struct Vz {
    artifact: BootArtifact,
    session_dir: PathBuf,
}

impl Vz {
    /// A backend that boots `artifact` and keeps each session's read-write
    /// disk under `session_dir`.
    pub fn new(artifact: BootArtifact, session_dir: impl Into<PathBuf>) -> Self {
        Self {
            artifact,
            session_dir: session_dir.into(),
        }
    }
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecTaskCreateFromSelf(allocator: *const c_void) -> *mut c_void;
    fn SecTaskCopyValueForEntitlement(
        task: *const c_void,
        entitlement: *const c_void,
        error: *mut c_void,
    ) -> *mut c_void;
}

/// Whether this process is signed with the one entitlement
/// Virtualization.framework demands, `com.apple.security.virtualization`.
///
/// This is the question [`detect`](crate::detect) asks before it will
/// answer with this backend.  A bare `cargo` binary says no; the signed
/// application, or a binary run through
/// `dev/scripts/sign-virtualization.sh`, says yes.
pub fn entitled() -> bool {
    let key = NSString::from_str("com.apple.security.virtualization");
    // SAFETY: both Security calls follow the CF create/copy rule, so each
    // non-null return is a +1 reference this function must balance.  CF and
    // Objective-C share one runtime — a CF object is an object — so each is
    // adopted into a `Retained`, whose drop releases it.  The rest is
    // toll-free bridging: an `NSString` reference is a `CFStringRef`, and a
    // boolean entitlement value comes back as a `CFBoolean`, which is an
    // `NSNumber`.
    unsafe {
        let Some(task) =
            Retained::from_raw(SecTaskCreateFromSelf(std::ptr::null()).cast::<NSObject>())
        else {
            return false;
        };
        let value = SecTaskCopyValueForEntitlement(
            (&raw const *task).cast(),
            (&raw const *key).cast(),
            std::ptr::null_mut(),
        );
        Retained::from_raw(value.cast::<NSObject>())
            .and_then(|value| value.downcast::<NSNumber>().ok())
            .is_some_and(|flag| flag.boolValue())
    }
}

impl Hypervisor for Vz {
    fn name(&self) -> &'static str {
        "Virtualization.framework"
    }

    /// Validate the spec, make the session disk, build and validate the
    /// `VZVirtualMachineConfiguration`, then start the machine on its own
    /// thread and wait for the guest to dial the control plane before
    /// declaring it booted.
    ///
    /// # Errors
    /// Returns [`Error`] if the spec is not usable (see
    /// [`MachineSpec::resolve`]), if the session disk cannot be made, if the
    /// configuration is refused by `validateWithError:`, or if the machine
    /// does not start and reach the guest within `BOOT_TIMEOUT`.
    fn boot(&self, spec: &MachineSpec) -> Result<Box<dyn Machine>, Error> {
        let folder = spec.resolve()?;
        let session = create_session_image(&self.session_dir)?;

        let plan = Plan {
            kernel: self.artifact.kernel.clone(),
            initramfs: self.artifact.initramfs.clone(),
            rootfs: self.artifact.rootfs.clone(),
            session,
            folder,
            guest_path: spec.workspace.guest_path.clone(),
            read_only: spec.workspace.read_only,
            vcpus: spec.vcpus,
            memory_mib: spec.memory_mib,
            epoch: unix_seconds(),
        };
        let mount_path = plan.guest_path.clone();

        let (ready_tx, ready_rx) =
            mpsc::sync_channel::<Result<(crate::Wires, Receiver<OwnedFd>), Error>>(1);
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let thread = thread::Builder::new()
            .name("ral-vz-machine".to_string())
            .spawn(move || run(&plan, &ready_tx, &cmd_rx))
            .map_err(|cause| Error::Unavailable {
                hypervisor: "Virtualization.framework",
                why: format!("could not start the thread that would own the machine: {cause}"),
            })?;

        match ready_rx.recv() {
            Ok(Ok((wires, agent_rx))) => Ok(Box::new(Guest {
                mount_path,
                wires: Some(wires),
                agent_rx,
                control: Some(Control {
                    cmd_tx,
                    thread: Some(thread),
                }),
            })),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(Error::Unavailable {
                    hypervisor: "Virtualization.framework",
                    why: "the machine thread ended before it reported whether the guest booted"
                        .to_string(),
                })
            }
        }
    }
}

/// Everything the worker thread needs to build and run one machine — all of
/// it `Send`, because the `VZ` objects it turns into are not and must be born
/// on the thread that owns them.
struct Plan {
    kernel: PathBuf,
    initramfs: PathBuf,
    rootfs: PathBuf,
    session: PathBuf,
    folder: PathBuf,
    guest_path: PathBuf,
    read_only: bool,
    vcpus: u8,
    memory_mib: u32,
    epoch: u64,
}

/// Build the machine's configuration from a [`Plan`].
///
/// This is the whole of the spec→framework mapping, and — unlike
/// [`validate`], which needs the virtualization entitlement — constructing the
/// configuration objects needs nothing special, so it is exercised directly by
/// the tests.  It creates every configuration object, wires the disk stack,
/// the share, the socket, and the console, leaves the network empty, and
/// clamps the resources.  Only opening a disk image or the shared folder can
/// fail here; structural soundness is [`validate`]'s question, asked before
/// the configuration reaches a machine.
///
/// # Errors
/// Returns [`Error::Unavailable`] carrying macOS's own words if a disk image
/// or the shared folder cannot be opened, or if the workspace tag is rejected.
fn build_configuration(plan: &Plan) -> Result<Retained<VZVirtualMachineConfiguration>, Error> {
    // SAFETY (whole function): these are ordinary Objective-C object
    // constructions and property setters on Virtualization.framework
    // configuration classes.  None of them start a machine or need the
    // virtualization entitlement; each `alloc`/`init` pair and each setter is
    // called with the argument types the SDK declares, and every `Retained`
    // is kept alive for as long as the configuration references it.
    unsafe {
        let config = VZVirtualMachineConfiguration::new();

        // Direct kernel boot: kernel + initramfs + the command line the guest
        // daemon reads its whole session from.
        let boot = VZLinuxBootLoader::initWithKernelURL(
            VZLinuxBootLoader::alloc(),
            &file_url(&plan.kernel),
        );
        boot.setInitialRamdiskURL(Some(&file_url(&plan.initramfs)));
        boot.setCommandLine(&NSString::from_str(&kernel_command_line(plan)));
        config.setBootLoader(Some(&boot));

        // The disk stack: read-only rootfs first, read-write session second.
        let rootfs = block_device(&plan.rootfs, true)?;
        let session = block_device(&plan.session, false)?;
        let storage: [&VZStorageDeviceConfiguration; 2] = [&**rootfs, &**session];
        config.setStorageDevices(&NSArray::from_slice(&storage));

        // The granted folder, shared under the tag the daemon mounts by.
        let tag = NSString::from_str(WORKSPACE_TAG);
        VZVirtioFileSystemDeviceConfiguration::validateTag_error(&tag).map_err(|error| {
            unavailable(&format!(
                "the workspace tag was refused: {}",
                ns_error(&error)
            ))
        })?;
        let sharing = VZVirtioFileSystemDeviceConfiguration::initWithTag(
            VZVirtioFileSystemDeviceConfiguration::alloc(),
            &tag,
        );
        let shared = VZSharedDirectory::initWithURL_readOnly(
            VZSharedDirectory::alloc(),
            &file_url(&plan.folder),
            plan.read_only,
        );
        let share =
            VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &shared);
        sharing.setShare(Some(&share));
        config.setDirectorySharingDevices(&NSArray::from_slice(&[&**sharing]));

        // The one socket device, port-routed for both wires — see `run`'s
        // two `setSocketListener_forPort` calls — and nothing else off the
        // machine.
        let socket = VZVirtioSocketDeviceConfiguration::new();
        config.setSocketDevices(&NSArray::from_slice(&[&**socket]));
        // Network devices are left empty on purpose: the guest has no way out
        // but the socket above.

        // The guest console, piped to the host's stdout where the daemon's
        // log lines belong.
        let console = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        let attachment =
            VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                VZFileHandleSerialPortAttachment::alloc(),
                None,
                Some(&NSFileHandle::fileHandleWithStandardOutput()),
            );
        console.setAttachment(Some(&attachment));
        config.setSerialPorts(&NSArray::from_slice(&[&**console]));

        config.setCPUCount(clamp_cpus(plan.vcpus));
        config.setMemorySize(clamp_memory(plan.memory_mib));

        Ok(config)
    }
}

/// Ask macOS whether the configuration is one it will run, before a machine is
/// ever built from it.
///
/// This is the entitlement wall in miniature: the framework refuses to
/// validate a configuration in a process that lacks
/// `com.apple.security.virtualization`, so on an unentitled binary — a bare
/// `cargo test`, this whole checkout — even a well-formed configuration is
/// turned away with that entitlement named.  On the signed application it is a
/// genuine structural check, and its complaint is the one message that says
/// which part of the configuration the framework refused.
///
/// # Errors
/// Returns [`Error::Unavailable`] carrying macOS's own words when the
/// configuration is refused — for want of the entitlement, or on its merits.
fn validate(config: &VZVirtualMachineConfiguration) -> Result<(), Error> {
    // SAFETY: `validateWithError:` inspects the configuration and reports an
    // `NSError` or nothing; it starts no machine.
    unsafe { config.validateWithError() }.map_err(|error| unavailable(&ns_error(&error)))
}

/// A `VZVirtioBlockDeviceConfiguration` over a raw disk image.
///
/// # Errors
/// Returns [`Error::Unavailable`] if the image cannot be opened — most often
/// because it is not there, which the tests rely on to prove the attachment
/// really touches the file.
fn block_device(
    image: &Path,
    read_only: bool,
) -> Result<Retained<VZVirtioBlockDeviceConfiguration>, Error> {
    // SAFETY: `initWithURL:readOnly:error:` opens the image at `image` and
    // returns either the attachment or an `NSError`; the URL is a valid file
    // URL and the error is surfaced, not ignored.  Wrapping it in a block
    // device configuration is an ordinary construction.
    unsafe {
        let attachment = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
            VZDiskImageStorageDeviceAttachment::alloc(),
            &file_url(image),
            read_only,
        )
        .map_err(|error| {
            unavailable(&format!(
                "the disk image {} could not be opened: {}",
                image.display(),
                ns_error(&error)
            ))
        })?;
        Ok(VZVirtioBlockDeviceConfiguration::initWithAttachment(
            VZVirtioBlockDeviceConfiguration::alloc(),
            &attachment,
        ))
    }
}

/// The kernel command line: the console, and the session settings the daemon
/// reads out of `/proc/cmdline`.  Rendered by `ral_daemon::boot::command_line`
/// rather than formatted here, so this backend and the daemon's own parser
/// cannot drift apart at compile time.
fn kernel_command_line(plan: &Plan) -> String {
    ral_daemon::boot::command_line(
        &ral_daemon::boot::Boot {
            workspace: ral_daemon::boot::Export::Virtiofs {
                tag: WORKSPACE_TAG.to_string(),
            },
            port: CONTROL_PORT,
            epoch: i64::try_from(plan.epoch).unwrap_or(i64::MAX),
            engine: ral_daemon::boot::DEFAULT_ENGINE.to_string(),
            net: Some(ral_daemon::boot::Net {
                port: crate::NET_PORT,
                address: crate::GUEST_LINK.address,
                prefix: crate::GUEST_LINK.prefix,
                gateway: crate::GUEST_LINK.gateway,
            }),
        },
        "hvc0",
    )
}

/// The spec's processor count, clamped into the framework's permitted range.
fn clamp_cpus(vcpus: u8) -> usize {
    // SAFETY: reads two class-level constants; no instance, no side effect.
    let (least, most) = unsafe {
        (
            VZVirtualMachineConfiguration::minimumAllowedCPUCount(),
            VZVirtualMachineConfiguration::maximumAllowedCPUCount(),
        )
    };
    usize::from(vcpus).clamp(least, most)
}

/// The spec's memory in bytes, clamped into the framework's permitted range.
/// A mebibyte count is already the whole-megabyte multiple the framework
/// requires, and the bounds it reports are multiples too, so clamping keeps
/// that property.
fn clamp_memory(memory_mib: u32) -> u64 {
    // SAFETY: reads two class-level constants; no instance, no side effect.
    let (least, most) = unsafe {
        (
            VZVirtualMachineConfiguration::minimumAllowedMemorySize(),
            VZVirtualMachineConfiguration::maximumAllowedMemorySize(),
        )
    };
    (u64::from(memory_mib) * 1024 * 1024).clamp(least, most)
}

/// A `file:` URL for a host path.
fn file_url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

/// macOS's own words for a framework error.
fn ns_error(error: &objc2_foundation::NSError) -> String {
    error.localizedDescription().to_string()
}

/// An [`Error::Unavailable`] for this backend carrying `why`.
fn unavailable(why: &str) -> Error {
    Error::Unavailable {
        hypervisor: "Virtualization.framework",
        why: why.to_string(),
    }
}

/// The host's wall clock now, in whole seconds since the Unix epoch, for the
/// guest to adopt as its own (`ral.epoch`).
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Make an empty per-session read-write disk of [`SESSION_IMAGE_BYTES`].
///
/// # Errors
/// Returns [`Error::Unavailable`] if the image cannot be created or sized.
#[allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: creating the per-session disk before any engine exists. \
              This is host-side session setup, not a model's turn-time write: there is no \
              Shell to route through a redirect door and no run to raise a card in, exactly \
              as with `MachineSpec::resolve`'s canonicalisation above."
)]
fn create_session_image(dir: &Path) -> Result<PathBuf, Error> {
    let path = dir.join(format!(
        "ral-session-{}-{}.img",
        std::process::id(),
        unix_seconds()
    ));
    let file = std::fs::File::create(&path).map_err(|cause| {
        unavailable(&format!(
            "the session disk {} could not be created: {cause}",
            path.display()
        ))
    })?;
    file.set_len(SESSION_IMAGE_BYTES).map_err(|cause| {
        unavailable(&format!(
            "the session disk {} could not be sized: {cause}",
            path.display()
        ))
    })?;
    Ok(path)
}

/// What the worker thread signals to itself while waiting for boot.
enum Ready {
    /// `start` finished — successfully, or with macOS's error text.
    Started(Result<(), String>),
    /// The guest dialed one of the two ports: it booted far enough to run
    /// init and open at least one of its wires.  The `u32` is the port that
    /// was dialed ([`CONTROL_PORT`] or [`crate::NET_PORT`]) and the [`OwnedFd`] is
    /// the host end of that connection, duplicated out of the framework's
    /// object to outlive it.
    Connected(u32, OwnedFd),
}

/// A message from a [`Guest`] to the thread that owns its machine.
enum Command {
    /// Bring the machine down, and report whether it stopped cleanly.
    Stop(SyncSender<Result<(), String>>),
    /// SPIKE: dial a port the *guest* listens on, the reverse of every wire
    /// this backend opens today. The machine is `!Send` and lives on the
    /// worker thread, so a connect has to arrive as a command like a stop.
    Connect {
        port: u32,
        reply: SyncSender<Result<OwnedFd, String>>,
    },
}

/// Be the thread that owns one machine, from build to power-off.
///
/// This is the code that needs the entitlement, and so the code this checkout
/// cannot run: it compiles, and its shape is the contract, but no line of it
/// past [`build_configuration`] has been exercised on an unentitled test box.
fn run(
    plan: &Plan,
    ready: &SyncSender<Result<(crate::Wires, Receiver<OwnedFd>), Error>>,
    commands: &Receiver<Command>,
) {
    let config = match build_configuration(plan).and_then(|config| {
        validate(&config)?;
        Ok(config)
    }) {
        Ok(config) => config,
        Err(error) => {
            let _ = ready.send(Err(error));
            let _ = remove_session_image(&plan.session);
            return;
        }
    };

    // The one serial queue the machine is spoken to on.  `None` asks GCD for
    // a serial queue, which is what Virtualization.framework requires.
    let queue = DispatchQueue::new("com.ral.vm-manager.vz", DispatchQueueAttr::SERIAL);
    // SAFETY: `initWithConfiguration:queue:` takes the validated configuration
    // and the serial queue the machine will forever be driven on; we create it
    // here and never move it off this thread.
    let machine = unsafe {
        VZVirtualMachine::initWithConfiguration_queue(VZVirtualMachine::alloc(), &config, &queue)
    };

    // Two listeners, one per wire, whose delegates wake us when the guest
    // dials each — not a problem needing two designs, the same one-shot
    // readiness answered twice.  Both are kept alive for the machine's whole
    // life: the device holds only weak references to them.
    let (events_tx, events_rx) = mpsc::channel::<Ready>();
    let control_delegate = SocketReadiness::new(CONTROL_PORT, events_tx.clone());
    let control_listener = new_listener(&control_delegate);
    let net_delegate = SocketReadiness::new(crate::NET_PORT, events_tx.clone());
    let net_listener = new_listener(&net_delegate);

    // The agent port's listener outlives boot: it is not part of the
    // readiness handshake above, and every hatched child dials it afresh
    // over the machine's whole life (see `AgentReadiness`).
    let (agent_tx, agent_rx) = mpsc::channel::<OwnedFd>();
    let agent_delegate = AgentReadiness::new(agent_tx);
    let agent_listener = new_agent_listener(&agent_delegate);

    let outcome = boot_machine(
        &queue,
        &machine,
        &control_listener,
        &net_listener,
        &agent_listener,
        &events_tx,
        &events_rx,
    );
    match outcome {
        Ok(wires) => {
            let _ = ready.send(Ok((wires, agent_rx)));
            wait_for_stop(&queue, &machine, commands);
        }
        Err(why) => {
            force_stop(&queue, &machine);
            let _ = ready.send(Err(unavailable(&why)));
        }
    }

    // Keep the delegates and listeners alive until the machine is done with
    // them, then let the session disk go.
    drop(control_listener);
    drop(control_delegate);
    drop(net_listener);
    drop(net_delegate);
    drop(agent_listener);
    drop(agent_delegate);
    let _ = remove_session_image(&plan.session);
}

/// Install both readiness listeners, start the machine, and wait for the
/// guest to dial both wires.
///
/// On success this yields the host ends of the guest's control-plane and net
/// connections — the frame protocol's stream and the packet pump's — for the
/// caller to keep.
///
/// # Errors
/// Returns macOS's start error, or a sentence naming whichever wire never
/// arrived, if the machine does not start or the guest does not dial both
/// within [`BOOT_TIMEOUT`].
fn boot_machine(
    queue: &DispatchQueue,
    machine: &VZVirtualMachine,
    control_listener: &VZVirtioSocketListener,
    net_listener: &VZVirtioSocketListener,
    agent_listener: &VZVirtioSocketListener,
    events_tx: &Sender<Ready>,
    events_rx: &Receiver<Ready>,
) -> Result<crate::Wires, String> {
    let machine_ptr = core::ptr::from_ref(machine);
    let control_listener_ptr = core::ptr::from_ref(control_listener);
    let net_listener_ptr = core::ptr::from_ref(net_listener);
    let agent_listener_ptr = core::ptr::from_ref(agent_listener);
    let start_tx = events_tx.clone();
    // The completion handler is copied and held by the framework; it captures
    // only the `Send`, `'static` sender.
    let completion = RcBlock::new(move |error: *mut objc2_foundation::NSError| {
        // SAFETY: the framework passes null on success or a borrowed NSError.
        let result = if error.is_null() {
            Ok(())
        } else {
            Err(ns_error(unsafe { &*error }))
        };
        let _ = start_tx.send(Ready::Started(result));
    });

    on_queue(queue, move || {
        // SAFETY: runs on the machine's own queue.  We fetch the runtime
        // socket device, attach all three readiness listeners to their
        // ports, and start the machine — every call on the queue it
        // demands, with the machine and listeners borrowed only for this
        // synchronous block.  One `VZVirtioSocketDeviceConfiguration`
        // already multiplexes every port, so this is three listener
        // registrations on the one device, not a second device.
        unsafe {
            let machine = &*machine_ptr;
            let control_listener = &*control_listener_ptr;
            let net_listener = &*net_listener_ptr;
            let agent_listener = &*agent_listener_ptr;
            let devices = machine.socketDevices();
            if let Some(device) = devices.firstObject()
                && let Ok(socket) = device.downcast::<VZVirtioSocketDevice>()
            {
                socket.setSocketListener_forPort(control_listener, CONTROL_PORT);
                socket.setSocketListener_forPort(net_listener, crate::NET_PORT);
                socket.setSocketListener_forPort(agent_listener, crate::AGENT_PORT);
            }
            machine.startWithCompletionHandler(&completion);
        }
    });

    let deadline = Instant::now() + BOOT_TIMEOUT;
    let mut control = None;
    let mut net = None;
    loop {
        if let (Some(_), Some(_)) = (&control, &net) {
            return Ok(crate::Wires {
                control: control.expect("checked above"),
                net: net.expect("checked above"),
            });
        }
        let left = deadline.saturating_duration_since(Instant::now());
        match events_rx.recv_timeout(left) {
            Ok(Ready::Connected(CONTROL_PORT, socket)) => control = Some(socket),
            Ok(Ready::Connected(crate::NET_PORT, socket)) => net = Some(socket),
            Ok(Ready::Connected(port, _)) => {
                return Err(format!("the guest dialed an unexpected port, {port}"));
            }
            Ok(Ready::Started(Ok(()))) => {}
            Ok(Ready::Started(Err(why))) => return Err(why),
            Err(RecvTimeoutError::Timeout) => {
                let missing = match (&control, &net) {
                    (None, None) => "either wire",
                    (None, Some(_)) => "the control plane",
                    (Some(_), None) => "the net wire",
                    (Some(_), Some(_)) => unreachable!("checked at the top of the loop"),
                };
                return Err(format!(
                    "the guest did not dial {missing} within {}s of starting: the kernel may \
                     have panicked before its init could run",
                    BOOT_TIMEOUT.as_secs()
                ));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(
                    "the machine's readiness channel closed before the guest connected".to_string(),
                );
            }
        }
    }
}

/// Wait for a stop command, or for the [`Guest`] to be dropped, then bring the
/// machine down.
fn wait_for_stop(queue: &DispatchQueue, machine: &VZVirtualMachine, commands: &Receiver<Command>) {
    let answer = loop {
        match commands.recv() {
            Ok(Command::Stop(answer)) => break Some(answer),
            Ok(Command::Connect { port, reply }) => {
                let _ = reply.send(connect_to_guest(queue, machine, port));
            }
            // The `Guest` was dropped: stop the machine anyway.
            Err(_) => break None,
        }
    };
    let result = stop_gracefully(queue, machine);
    if let Some(answer) = answer {
        let _ = answer.send(result);
    }
}

/// How long to wait for `connectToPort:`'s completion handler.
///
/// A backstop, not the normal path. The framework's own documentation says it
/// *"does nothing if the guest does not listen on that port"*, which would make
/// this deadline load-bearing — but measured against a real guest, a port with
/// no listener answers `ECONNRESET` from the guest's own vsock stack in ~25ms.
/// The deadline is kept for the case the docs describe and the machine did not.
const GUEST_DIAL_PATIENCE: Duration = Duration::from_secs(5);

/// SPIKE: dial `port` on the guest and hand back the host end.
///
/// The inverse of every wire this backend opens today: `setSocketListener_forPort`
/// waits for the guest to dial the host, this asks the host to dial the guest.
/// Requires something in the guest to be listening — no such listener exists
/// yet, which is the other half of the work this spike measures.
fn connect_to_guest(
    queue: &DispatchQueue,
    machine: &VZVirtualMachine,
    port: u32,
) -> Result<OwnedFd, String> {
    let machine_ptr = core::ptr::from_ref(machine);
    // Rendezvous rather than a plain channel: the completion handler runs on
    // the machine's queue *after* `on_queue` returns, so the sender must
    // outlive this call's synchronous part.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<Result<OwnedFd, String>>();

    on_queue(queue, move || {
        let done_tx = done_tx.clone();
        // SAFETY: on the machine's queue. `socketDevices` is the same
        // property the boot block reads; the completion block is retained by
        // the framework for as long as the connect is outstanding, which is
        // why an `RcBlock` created here may be dropped when this block ends.
        unsafe {
            let machine = &*machine_ptr;
            let devices = machine.socketDevices();
            let Some(device) = devices.firstObject() else {
                let _ = done_tx.send(Err("the machine has no virtio socket device".to_string()));
                return;
            };
            let Ok(socket) = device.downcast::<VZVirtioSocketDevice>() else {
                let _ = done_tx.send(Err("the machine's socket device is not virtio".to_string()));
                return;
            };
            let completion = RcBlock::new(
                move |connection: *mut VZVirtioSocketConnection,
                      error: *mut objc2_foundation::NSError| {
                    // The same dup the accept path performs, and for the same
                    // reason: VZ may close its connection object once this
                    // handler returns.
                    let outcome = if let Some(connection) = connection.as_ref() {
                        BorrowedFd::borrow_raw(connection.fileDescriptor())
                            .try_clone_to_owned()
                            .map_err(|e| format!("could not dup the guest connection: {e}"))
                    } else if let Some(error) = error.as_ref() {
                        Err(format!("the guest refused the dial: {}", error.localizedDescription()))
                    } else {
                        Err("the connect completed with neither a connection nor an error".to_string())
                    };
                    let _ = done_tx.send(outcome);
                },
            );
            socket.connectToPort_completionHandler(port, &completion);
        }
    });

    done_rx
        .recv_timeout(GUEST_DIAL_PATIENCE)
        .unwrap_or_else(|_| {
            Err(format!(
                "nothing in the guest answered a dial to port {port} within {}s — the framework \
                 treats a missing listener as a no-op rather than an error, so this is what that \
                 looks like",
                GUEST_DIAL_PATIENCE.as_secs()
            ))
        })
}

/// Ask the guest to shut down, wait for the machine to come to rest, and only
/// then force it — the order [`Machine::shutdown`]'s contract fixes.
///
/// # Errors
/// Returns a sentence if the machine had to be forced after the guest failed
/// to stop within [`STOP_GRACE`].
fn stop_gracefully(queue: &DispatchQueue, machine: &VZVirtualMachine) -> Result<(), String> {
    request_stop(queue, machine);
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline {
        match read_state(queue, machine) {
            VZVirtualMachineState::Stopped => return Ok(()),
            VZVirtualMachineState::Error => {
                return Err("the machine stopped in an error state".to_string());
            }
            _ => thread::sleep(STOP_PULSE),
        }
    }
    force_stop(queue, machine);
    Err("the guest did not stop within the grace period; the machine was forced".to_string())
}

/// Send the guest a stop request over the control plane, if it will take one.
fn request_stop(queue: &DispatchQueue, machine: &VZVirtualMachine) {
    let machine_ptr = core::ptr::from_ref(machine);
    on_queue(queue, move || {
        // SAFETY: on the machine's queue; `requestStopWithError` is the
        // polite ACPI-style stop, guarded by `canRequestStop`.
        unsafe {
            let machine = &*machine_ptr;
            if machine.canRequestStop() {
                let _ = machine.requestStopWithError();
            }
        }
    });
}

/// Force the machine off, ignoring the outcome — the last resort behind
/// [`stop_gracefully`] and the error paths.
fn force_stop(queue: &DispatchQueue, machine: &VZVirtualMachine) {
    let machine_ptr = core::ptr::from_ref(machine);
    on_queue(queue, move || {
        // SAFETY: on the machine's queue; `stopWithCompletionHandler` is the
        // destructive stop, valid only from Running or Paused, so it is
        // guarded by `canStop`.  The completion is a no-op sink.
        unsafe {
            let machine = &*machine_ptr;
            if machine.canStop() {
                let sink = RcBlock::new(|_error: *mut objc2_foundation::NSError| {});
                machine.stopWithCompletionHandler(&sink);
            }
        }
    });
}

/// Read the machine's state on its queue.
fn read_state(queue: &DispatchQueue, machine: &VZVirtualMachine) -> VZVirtualMachineState {
    let machine_ptr = core::ptr::from_ref(machine);
    let slot = AtomicIsize::new(VZVirtualMachineState::Stopped.0);
    let slot_ptr = core::ptr::from_ref(&slot);
    on_queue(queue, move || {
        // SAFETY: on the machine's queue; `state` is a plain property read,
        // and the result is parked in the atomic this synchronous block
        // borrows for exactly its own duration.
        let state = unsafe { (*machine_ptr).state() };
        unsafe { &*slot_ptr }.store(state.0, Ordering::SeqCst);
    });
    VZVirtualMachineState(slot.load(Ordering::SeqCst))
}

/// Run `body` synchronously on `queue`.
///
/// `dispatch_sync` runs the block before returning, so a block that borrows a
/// `!Send` machine through a raw pointer is safe: the borrow cannot outlive
/// this call, and the block never crosses to another thread.  This is the one
/// primitive the whole thread model rests on, and the reason a machine that
/// Rust will not let us *send* can still be *driven* from Rust.
fn on_queue(queue: &DispatchQueue, body: impl Fn() + 'static) {
    let block = RcBlock::new(body);
    let block: *mut DynBlock<dyn Fn()> = core::ptr::from_ref(&*block).cast_mut();
    // SAFETY: `block` points at a live block for the duration of this call,
    // `dispatch_sync` runs it exactly once and does not retain it past return,
    // and the pointer is non-null.
    unsafe { queue.exec_sync_with_block(block) };
}

/// A fresh `VZVirtioSocketListener` whose delegate is `readiness`.
fn new_listener(readiness: &Retained<SocketReadiness>) -> Retained<VZVirtioSocketListener> {
    // SAFETY: constructs a listener and sets its (weak) delegate to the
    // readiness object, which the caller keeps alive.
    unsafe {
        let listener = VZVirtioSocketListener::new();
        listener.setDelegate(Some(ProtocolObject::from_ref(&**readiness)));
        listener
    }
}

/// A fresh `VZVirtioSocketListener` whose delegate is `readiness` — the
/// agent port's twin of [`new_listener`], kept separate only because its
/// delegate is a different Objective-C class.
fn new_agent_listener(readiness: &Retained<AgentReadiness>) -> Retained<VZVirtioSocketListener> {
    // SAFETY: as `new_listener` above.
    unsafe {
        let listener = VZVirtioSocketListener::new();
        listener.setDelegate(Some(ProtocolObject::from_ref(&**readiness)));
        listener
    }
}

/// Remove a session disk once its machine is gone, best-effort.
#[allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: releasing the per-session disk after power-off. Infrastructure \
              teardown, not a model's turn-time write; no Shell, no run, no card."
)]
fn remove_session_image(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

/// The delegate object that turns the guest's first connection on one wire
/// into a [`Ready::Connected`] carrying the port and the host end of that
/// connection.  One instance per wire — the control plane gets one, the net
/// wire another — each spent at most once, for the one connection its port
/// ever accepts.
struct SocketReadinessIvars {
    port: u32,
    announce: Mutex<Option<Sender<Ready>>>,
}

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements beyond those the delegate
    //   protocol adds, which are satisfied below.
    // - `SocketReadiness` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "RalVzSocketReadiness"]
    #[ivars = SocketReadinessIvars]
    struct SocketReadiness;

    unsafe impl NSObjectProtocol for SocketReadiness {}

    unsafe impl VZVirtioSocketListenerDelegate for SocketReadiness {
        #[unsafe(method(listener:shouldAcceptNewConnection:fromSocketDevice:))]
        fn should_accept(
            &self,
            _listener: &VZVirtioSocketListener,
            connection: &VZVirtioSocketConnection,
            _device: &VZVirtioSocketDevice,
        ) -> bool {
            // The accepted `VZVirtioSocketConnection` is the stream the
            // control-plane frame protocol runs over, so its descriptor is
            // duplicated before anything else: VZ may close the connection
            // object once this delegate returns.
            //
            // SAFETY: `fileDescriptor` reads the connection's live descriptor,
            // valid for the duration of this delegate call; `borrow_raw`
            // borrows it for no longer, and `try_clone_to_owned` dups it (with
            // `CLOEXEC`) into an `OwnedFd` that outlives the connection.
            let dup = (unsafe { BorrowedFd::borrow_raw(connection.fileDescriptor()) })
                .try_clone_to_owned();

            // Only on a successful dup is the one-shot announce spent, and
            // only then is the connection accepted.
            if let Ok(control_socket) = dup
                && let Ok(mut slot) = self.ivars().announce.lock()
                && let Some(announce) = slot.take()
            {
                let _ = announce.send(Ready::Connected(self.ivars().port, control_socket));
                true
            } else {
                // Refuse.  Each way here is benign: a failed dup leaves the
                // announce unspent, so the guest daemon redials within its
                // patience window (`ral-daemon`'s `control_plane`) and the
                // next connection may dup where this one could not; an already
                // spent announce means a machine's one control connection is
                // taken, so a second dial is an anomaly to turn away.  The
                // dup, if there was one, is dropped and closed with this
                // branch.
                false
            }
        }
    }
);

impl SocketReadiness {
    /// A delegate for `port` that will send `Ready::Connected` down
    /// `announce` the first time the guest connects to it.
    fn new(port: u32, announce: Sender<Ready>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(SocketReadinessIvars {
            port,
            announce: Mutex::new(Some(announce)),
        });
        // SAFETY: `super(this), init` is `NSObject`'s designated initializer
        // on a freshly allocated instance whose ivars are set.
        unsafe { msg_send![super(this), init] }
    }
}

/// The agent port's delegate: [`SocketReadiness`]'s persistent cousin.
///
/// Where the control and net ports each announce exactly one connection and
/// then go quiet, the agent port's whole meaning is that dials keep coming
/// — every hatched child opens a fresh one over the machine's life — so this
/// delegate never spends its sender. Each accepted connection's descriptor
/// is duplicated (VZ may close the connection object once this delegate
/// returns, precisely as [`SocketReadiness`] guards against) and pushed into
/// an `mpsc::Sender`, whose other end is [`Guest::accept_agent`]'s receiver.
struct AgentReadinessIvars {
    accepted: Sender<OwnedFd>,
}

define_class!(
    // SAFETY: as `SocketReadiness` above.
    #[unsafe(super(NSObject))]
    #[name = "RalVzAgentReadiness"]
    #[ivars = AgentReadinessIvars]
    struct AgentReadiness;

    unsafe impl NSObjectProtocol for AgentReadiness {}

    unsafe impl VZVirtioSocketListenerDelegate for AgentReadiness {
        #[unsafe(method(listener:shouldAcceptNewConnection:fromSocketDevice:))]
        fn should_accept(
            &self,
            _listener: &VZVirtioSocketListener,
            connection: &VZVirtioSocketConnection,
            _device: &VZVirtioSocketDevice,
        ) -> bool {
            // SAFETY: as `SocketReadiness::should_accept` — the descriptor is
            // borrowed only for the duration of this call and duplicated
            // before the connection object might close.
            let dup = (unsafe { BorrowedFd::borrow_raw(connection.fileDescriptor()) })
                .try_clone_to_owned();
            match dup {
                // A send failing means nothing is receiving any more — the
                // machine is tearing down — so refusing is the honest answer.
                Ok(fd) => self.ivars().accepted.send(fd).is_ok(),
                Err(_) => false,
            }
        }
    }
);

impl AgentReadiness {
    /// A delegate that pushes every accepted connection's duplicated
    /// descriptor down `accepted`.
    fn new(accepted: Sender<OwnedFd>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(AgentReadinessIvars { accepted });
        // SAFETY: as `SocketReadiness::new` above.
        unsafe { msg_send![super(this), init] }
    }
}

/// A booted guest, held open by the thread that owns its machine.
///
/// This type carries no `VZ` object — those live on the machine thread and are
/// spoken to only by its `Command` channel — so it is free to move between
/// the caller's threads.
pub struct Guest {
    /// The path *inside* the guest at which virtiofs mounted the shared
    /// folder — what [`Machine::workspace_path`] hands back.
    mount_path: PathBuf,
    /// The two wires accepted at boot.  Taken out by
    /// [`Machine::take_wires`], which may be called at most once.
    wires: Option<crate::Wires>,
    /// Every guest dial on the agent port, in the order the machine thread's
    /// [`AgentReadiness`] delegate accepted them.  Read by
    /// [`Machine::accept_agent`], which may be called any number of times.
    agent_rx: Receiver<OwnedFd>,
    control: Option<Control>,
}

/// The live end of a machine's thread: the channel to it, and the thread
/// itself, kept so a stop can be both asked for and waited on.  Not to be
/// confused with [`Wires::control`](crate::Wires::control), the
/// control-plane wire held in [`Guest::wires`].
struct Control {
    cmd_tx: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl Guest {
    /// Ask the machine thread to stop, wait for its answer, and join it.
    ///
    /// Idempotent through the `Option`: [`Machine::shutdown`] takes the
    /// control out, so the `Drop` that follows finds nothing left to do.
    fn stop(&mut self) -> Result<(), Error> {
        // Close the host ends of both wires first, if they were never taken:
        // the guest's engine exits on EOF and the daemon powers the machine
        // off from inside — the same inside-out shutdown the end of a
        // session performs — so the grace window below normally observes a
        // stop rather than forcing one.
        self.wires = None;
        let Some(mut control) = self.control.take() else {
            return Ok(());
        };
        let (answer_tx, answer_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        // If the thread has already gone, the send fails and the join below
        // still tells us it is down; either way there is nothing to force.
        let result = if control.cmd_tx.send(Command::Stop(answer_tx)).is_ok() {
            match answer_rx.recv() {
                Ok(Err(why)) => Err(unavailable(&why)),
                Ok(Ok(())) | Err(_) => Ok(()),
            }
        } else {
            Ok(())
        };
        if let Some(thread) = control.thread.take() {
            let _ = thread.join();
        }
        result
    }
}

impl Machine for Guest {
    /// The path *inside* the guest at which virtiofs mounted the shared
    /// folder.  Never a host path: under this backend the host's own path is
    /// exactly what the agent must not be able to name.
    fn workspace_path(&self) -> &Path {
        &self.mount_path
    }

    /// The two wires the guest dialed at boot, handed over once.
    ///
    /// # Panics
    /// Panics if asked a second time: a machine has exactly one of each.
    fn take_wires(&mut self) -> crate::Wires {
        self.wires
            .take()
            .expect("a machine's wires are taken at most once")
    }

    /// Wait up to `patience` for the next guest dial on the agent port.
    ///
    /// # Errors
    /// Returns [`io::ErrorKind::TimedOut`] if none arrives within `patience`,
    /// or [`io::ErrorKind::BrokenPipe`] if the machine has torn down its
    /// agent listener (the sender dropped alongside it).
    fn accept_agent(&mut self, patience: std::time::Duration) -> std::io::Result<crate::AgentDial> {
        self.agent_rx
            .recv_timeout(patience)
            .map_err(|cause| match cause {
                RecvTimeoutError::Timeout => std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "no guest dialed the agent port within the patience given",
                ),
                RecvTimeoutError::Disconnected => std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "the machine's agent listener is gone",
                ),
            })
    }

    /// Dial `port` inside the guest and hand back the host end.
    ///
    /// # Errors
    /// Returns a sentence if no listener answers — a guest with nothing bound
    /// resets the connection, so this is prompt rather than a wait — or if the
    /// machine thread is already gone.
    fn connect_guest(&self, port: u32) -> std::io::Result<crate::AgentDial> {
        self.dial_guest(port)
            .map_err(|why| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, why))
    }

    /// Ask the guest daemon to shut down, wait for the machine to reach the
    /// stopped state, and only then force it; the session disk is released by
    /// the machine thread as its last act, after the machine is down.
    ///
    /// # Errors
    /// Returns [`Error::Unavailable`] if the guest did not stop cleanly and
    /// the machine had to be forced.
    fn shutdown(mut self: Box<Self>) -> Result<(), Error> {
        self.stop()
    }
}

impl Guest {
    /// The body of [`Machine::connect_guest`], in this backend's own error
    /// vocabulary; the trait arm re-dresses it as `io::Error`.
    fn dial_guest(&self, port: u32) -> Result<OwnedFd, String> {
        let control = self
            .control
            .as_ref()
            .ok_or_else(|| "the machine thread is already gone".to_string())?;
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(0);
        control
            .cmd_tx
            .send(Command::Connect {
                port,
                reply: reply_tx,
            })
            .map_err(|_| "the machine thread stopped before the dial was sent".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "the machine thread stopped while dialling the guest".to_string())?
    }
}

impl Drop for Guest {
    /// Dropping a machine stops it too, so a caller that forgets — or unwinds
    /// — does not leave a guest running.
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: test scaffolding writes the dummy kernel, images, and folder it \
              then builds a configuration from; no shell, no run, no card."
)]
mod tests {
    use super::*;

    /// A plan over freshly made dummy files: a real (empty) kernel, initramfs,
    /// rootfs, and session image, and a real granted folder.  Enough for the
    /// configuration to be built and validated without a boot.
    struct Fixture {
        _dir: tempfile::TempDir,
        plan: Plan,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let make = |name: &str, bytes: u64| {
            let path = dir.path().join(name);
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(bytes).unwrap();
            path
        };
        let folder = dir.path().join("work");
        std::fs::create_dir(&folder).unwrap();
        let plan = Plan {
            kernel: make("kernel", 4096),
            initramfs: make("initramfs", 4096),
            rootfs: make("rootfs.img", 64 * 1024 * 1024),
            session: make("session.img", SESSION_IMAGE_BYTES),
            folder,
            guest_path: PathBuf::from(MachineSpec::GUEST_WORKSPACE),
            read_only: false,
            vcpus: 2,
            memory_mib: 2048,
            epoch: 1_771_200_000,
        };
        Fixture { _dir: dir, plan }
    }

    /// The whole spec→framework mapping is real: a well-formed plan builds a
    /// configuration, and validation turns it away only for the entitlement
    /// this test binary cannot have — never on the configuration's own merits.
    /// On the signed application the same call returns `Ok`; either way, a
    /// well-formed configuration is not rejected as malformed.
    #[test]
    fn a_well_formed_plan_builds_and_is_refused_only_for_the_entitlement() {
        let fixture = fixture();
        let config = build_configuration(&fixture.plan).expect("a well-formed plan must build");
        match validate(&config) {
            Ok(()) => {}
            Err(Error::Unavailable { why, .. }) => assert!(
                why.contains("entitlement"),
                "a well-formed configuration must fail only for the entitlement, not: {why}"
            ),
            Err(other) => panic!("validation failed for an unexpected reason: {other}"),
        }
    }

    /// The configuration carries exactly the machine the design asks for: two
    /// disks, one share, one socket device carrying both the control plane
    /// and the net wire, and — the load-bearing absence — no network device
    /// at all.  This reads the objects back before validation, so it needs no
    /// entitlement.
    ///
    /// `socketDevices().count() == 1` is asserted deliberately, alongside the
    /// `networkDevices` absence: a `VZVirtioSocketDeviceConfiguration`
    /// already multiplexes ports, so a second socket device would be a
    /// regression the next reader might mistake for how a second wire is
    /// meant to be added.
    #[test]
    fn the_configuration_is_the_machine_the_design_describes() {
        let fixture = fixture();
        let config = build_configuration(&fixture.plan).unwrap();
        // SAFETY: reading back the properties just set, off any queue: the
        // configuration is not a running machine and has no queue affinity.
        unsafe {
            assert_eq!(config.storageDevices().count(), 2, "rootfs and session");
            assert_eq!(config.directorySharingDevices().count(), 1, "the share");
            assert_eq!(
                config.socketDevices().count(),
                1,
                "one socket device carries both the control plane and the net wire"
            );
            assert_eq!(config.serialPorts().count(), 1, "the console");
            assert_eq!(
                config.networkDevices().count(),
                0,
                "the guest has no way out but the socket"
            );
        }
    }

    /// The resources land inside the framework's permitted range, and the
    /// memory stays a whole-megabyte multiple.
    #[test]
    fn resources_are_clamped_into_the_permitted_range() {
        let cpus = clamp_cpus(2);
        // SAFETY: class-level range reads.
        let (least_cpu, most_cpu) = unsafe {
            (
                VZVirtualMachineConfiguration::minimumAllowedCPUCount(),
                VZVirtualMachineConfiguration::maximumAllowedCPUCount(),
            )
        };
        assert!((least_cpu..=most_cpu).contains(&cpus));

        let memory = clamp_memory(2048);
        assert_eq!(memory % (1024 * 1024), 0, "memory is a whole-megabyte size");
    }

    /// An absent disk image is caught where it is opened, so the attachment
    /// really touches the file rather than merely naming it.
    #[test]
    fn a_missing_disk_image_is_refused_by_the_attachment() {
        let error = block_device(Path::new("/nowhere/at/all/rootfs.img"), true)
            .expect_err("a missing image cannot attach");
        assert!(matches!(error, Error::Unavailable { .. }), "{error}");
    }

    /// The command line carries the console and the session settings the
    /// daemon reads, spelled the way `ral-daemon`'s `boot::Boot::read` expects
    /// — including the net wire's composite key.
    #[test]
    fn the_command_line_addresses_the_daemon() {
        let fixture = fixture();
        let line = kernel_command_line(&fixture.plan);
        assert!(line.contains("console=hvc0"), "{line}");
        assert!(line.contains("ral.workspace=workspace"), "{line}");
        assert!(line.contains("ral.port=1729"), "{line}");
        assert!(line.contains("ral.epoch=1771200000"), "{line}");
        assert!(
            line.contains("ral.net=1730,10.0.2.15/24,10.0.2.2"),
            "{line}"
        );
    }

    /// A machine has exactly one of each wire: [`take_wires`] hands both host
    /// ends over once, together.  Built here from socketpairs rather than a
    /// boot, since the wires are all the law concerns.
    ///
    /// [`take_wires`]: Machine::take_wires
    #[test]
    fn the_wires_are_handed_over_once() {
        let (control, _guest_control) = std::os::unix::net::UnixStream::pair().unwrap();
        let (net, _guest_net) = std::os::unix::net::UnixStream::pair().unwrap();
        let (_agent_tx, agent_rx) = mpsc::channel();
        let mut guest = Guest {
            mount_path: PathBuf::from(MachineSpec::GUEST_WORKSPACE),
            wires: Some(crate::Wires {
                control: OwnedFd::from(control),
                net: OwnedFd::from(net),
            }),
            agent_rx,
            control: None,
        };
        let _wires = guest.take_wires();
    }

    /// A second ask finds nothing left to hand over: it is a caller's own
    /// error, so the machine panics rather than answer with used-up wires.
    #[test]
    #[should_panic(expected = "at most once")]
    fn a_second_ask_for_the_wires_panics() {
        let (control, _guest_control) = std::os::unix::net::UnixStream::pair().unwrap();
        let (net, _guest_net) = std::os::unix::net::UnixStream::pair().unwrap();
        let (_agent_tx, agent_rx) = mpsc::channel();
        let mut guest = Guest {
            mount_path: PathBuf::from(MachineSpec::GUEST_WORKSPACE),
            wires: Some(crate::Wires {
                control: OwnedFd::from(control),
                net: OwnedFd::from(net),
            }),
            agent_rx,
            control: None,
        };
        let _ = guest.take_wires();
        let _ = guest.take_wires();
    }

    /// A guest with no agent dial waiting times out honestly, rather than
    /// hanging or panicking — the substrate's proof, independent of a real
    /// boot, that [`Machine::accept_agent`] behaves when nothing has dialed.
    #[test]
    fn accept_agent_times_out_when_nothing_has_dialed() {
        let (control, _guest_control) = std::os::unix::net::UnixStream::pair().unwrap();
        let (net, _guest_net) = std::os::unix::net::UnixStream::pair().unwrap();
        let (_agent_tx, agent_rx) = mpsc::channel();
        let mut guest = Guest {
            mount_path: PathBuf::from(MachineSpec::GUEST_WORKSPACE),
            wires: Some(crate::Wires {
                control: OwnedFd::from(control),
                net: OwnedFd::from(net),
            }),
            agent_rx,
            control: None,
        };
        let error = guest
            .accept_agent(Duration::from_millis(20))
            .expect_err("nothing dialed");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    /// A machine may accept the agent port more than once — the law that
    /// tells it apart from the one-shot control and net wires. Two dials
    /// queued ahead of time come back in order, and a caller may go on
    /// asking after that.
    #[test]
    fn accept_agent_hands_back_every_dial_in_order() {
        let (control, _guest_control) = std::os::unix::net::UnixStream::pair().unwrap();
        let (net, _guest_net) = std::os::unix::net::UnixStream::pair().unwrap();
        let (agent_tx, agent_rx) = mpsc::channel();
        let (first, _first_peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let (second, _second_peer) = std::os::unix::net::UnixStream::pair().unwrap();
        agent_tx.send(OwnedFd::from(first)).unwrap();
        agent_tx.send(OwnedFd::from(second)).unwrap();
        let mut guest = Guest {
            mount_path: PathBuf::from(MachineSpec::GUEST_WORKSPACE),
            wires: Some(crate::Wires {
                control: OwnedFd::from(control),
                net: OwnedFd::from(net),
            }),
            agent_rx,
            control: None,
        };
        guest
            .accept_agent(Duration::from_millis(20))
            .expect("the first dial");
        guest
            .accept_agent(Duration::from_millis(20))
            .expect("the second dial");
        assert_eq!(
            guest
                .accept_agent(Duration::from_millis(20))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::TimedOut,
            "a third ask finds nothing queued"
        );
    }

    /// The backend names itself, for the greeting a user sees.
    #[test]
    fn the_backend_names_itself() {
        let vz = Vz::new(
            BootArtifact {
                kernel: PathBuf::from("/boot/kernel"),
                initramfs: PathBuf::from("/boot/initramfs"),
                rootfs: PathBuf::from("/boot/rootfs.img"),
            },
            "/tmp",
        );
        assert_eq!(vz.name(), "Virtualization.framework");
    }
}
