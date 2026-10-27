//! Every !Send VZ object lives on one worker; machine operations use its serial queue.
//!
//! Listeners and weak delegates outlive the running machine; boot waits for both guest dials.
//! Validation and boot require the process virtualization entitlement.

use std::ffi::c_void;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use guest_abi::ports;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSNumber, NSObject, NSString};
use objc2_virtualization::{
    VZVirtioSocketDevice, VZVirtioSocketListener, VZVirtualMachine, VZVirtualMachineState,
};

use crate::error::{Error, GuestConsole, Wire};
use crate::lifecycle::{Leaf, Missed, SessionDisk, await_dials, stop_within, sweep, unix_seconds};
use crate::machine::{self, Booted, Hypervisor, Machine, Socket};
use crate::spec::{BootArtifact, MachineSpec, SIZE};

mod config;
mod queue;

use config::{Plan, SESSION_IMAGE_BYTES, build_configuration, ns_error, validate};
use queue::{
    Ready, SocketReadiness, dial, force_stop, new_listener, on_queue, read_state, request_stop,
};

const HYPERVISOR: &str = "Virtualization.framework";

pub struct Vz {
    artifact: BootArtifact,
    session_dir: PathBuf,
}

impl Vz {
    pub fn new(artifact: BootArtifact, session_dir: impl Into<PathBuf>) -> Self {
        let session_dir = session_dir.into();
        sweep(&session_dir);
        Self {
            artifact,
            session_dir,
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

pub fn entitled() -> bool {
    let key = NSString::from_str("com.apple.security.virtualization");
    // SAFETY: Security returns owned CF objects; Retained balances each non-null reference.
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
    /// # Errors
    /// Returns the spec, session-disk, configuration or guest-boot refusal.
    fn boot(&self, spec: &MachineSpec) -> Result<Booted, Error> {
        let folder = spec.resolve()?;
        let session = SessionDisk::create(&self.session_dir, Leaf::Raw, |disk| {
            disk.set_len(SESSION_IMAGE_BYTES)
        })?;

        let plan = Plan {
            kernel: self.artifact.kernel.clone(),
            initramfs: self.artifact.initramfs.clone(),
            rootfs: self.artifact.rootfs.clone(),
            session,
            folder,
            read_only: spec.read_only,
            vcpus: SIZE.vcpus,
            memory_mib: SIZE.memory_mib,
            epoch: unix_seconds(),
        };

        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<machine::Wires, Error>>(1);
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let thread = thread::Builder::new()
            .name("ral-vz-machine".to_string())
            .spawn(move || run(&plan, &ready_tx, &cmd_rx))
            .map_err(|cause| {
                Error::backend(
                    HYPERVISOR,
                    format!("could not start the thread that would own the machine: {cause}"),
                )
            })?;

        let outcome = ready_rx.recv().unwrap_or_else(|_| {
            Err(Error::backend(
                HYPERVISOR,
                "the machine thread ended before it reported whether the guest booted",
            ))
        });
        match outcome {
            Ok(wires) => Ok(Booted {
                machine: Box::new(Guest {
                    control: Some(Control { cmd_tx, thread }),
                }),
                wires,
            }),
            Err(error) => {
                let _ = thread.join();
                Err(error)
            }
        }
    }
}

enum Command {
    Stop(SyncSender<Result<(), Error>>),
    /// Dial on the worker thread, where the !Send machine lives.
    Connect {
        port: u32,
        reply: SyncSender<std::io::Result<OwnedFd>>,
    },
}

fn run(
    plan: &Plan,
    ready: &SyncSender<Result<machine::Wires, Error>>,
    commands: &Receiver<Command>,
) {
    let config = match build_configuration(plan).and_then(|config| {
        validate(&config)?;
        Ok(config)
    }) {
        Ok(config) => config,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    // Virtualization.framework requires one serial dispatch queue per machine.
    let queue = DispatchQueue::new("com.ral.vm-manager.vz", DispatchQueueAttr::SERIAL);
    // SAFETY: the validated configuration and serial queue live on this worker thread.
    let machine = unsafe {
        VZVirtualMachine::initWithConfiguration_queue(VZVirtualMachine::alloc(), &config, &queue)
    };

    // Both listeners and their weakly held delegates outlive the running machine.
    let (events_tx, events_rx) = mpsc::channel::<Ready>();
    let control_delegate = SocketReadiness::new(Wire::Control, events_tx.clone());
    let control_listener = new_listener(&control_delegate);
    let net_delegate = SocketReadiness::new(Wire::Net, events_tx.clone());
    let net_listener = new_listener(&net_delegate);

    let outcome = boot_machine(
        &queue,
        &machine,
        &control_listener,
        &net_listener,
        &events_tx,
        &events_rx,
    );
    match outcome {
        Ok(wires) => {
            let _ = ready.send(Ok(wires));
            wait_for_stop(&queue, &machine, commands);
        }
        Err(error) => {
            force_stop(&queue, &machine);
            let _ = ready.send(Err(error));
        }
    }
}

/// # Errors
/// Returns the framework's start refusal or a guest-dial timeout.
fn boot_machine(
    queue: &DispatchQueue,
    machine: &VZVirtualMachine,
    control_listener: &VZVirtioSocketListener,
    net_listener: &VZVirtioSocketListener,
    events_tx: &Sender<Ready>,
    events_rx: &Receiver<Ready>,
) -> Result<machine::Wires, Error> {
    let machine_ptr = core::ptr::from_ref(machine);
    let control_listener_ptr = core::ptr::from_ref(control_listener);
    let net_listener_ptr = core::ptr::from_ref(net_listener);
    let start_tx = events_tx.clone();
    // The completion handler is copied and held by the framework; it captures
    // only the `Send`, `'static` sender.
    let completion = RcBlock::new(move |error: *mut objc2_foundation::NSError| {
        // SAFETY: the framework passes null on success or a borrowed NSError.
        let result = if error.is_null() {
            Ok(())
        } else {
            Err(Error::backend(HYPERVISOR, ns_error(unsafe { &*error })))
        };
        let _ = start_tx.send(Ready::Started(result));
    });

    on_queue(queue, move || {
        // SAFETY: this synchronous queue call borrows the machine and both listeners through start.
        unsafe {
            let machine = &*machine_ptr;
            let control_listener = &*control_listener_ptr;
            let net_listener = &*net_listener_ptr;
            let devices = machine.socketDevices();
            if let Some(device) = devices.firstObject()
                && let Ok(socket) = device.downcast::<VZVirtioSocketDevice>()
            {
                for (wire, listener) in
                    [(Wire::Control, control_listener), (Wire::Net, net_listener)]
                {
                    let port = match wire {
                        Wire::Control => ports::CONTROL,
                        Wire::Net => ports::NET,
                    };
                    socket.setSocketListener_forPort(listener, port);
                }
            }
            machine.startWithCompletionHandler(&completion);
        }
    });

    let mut control = None;
    let mut net = None;
    await_dials(
        HYPERVISOR,
        |wire, patience| {
            let deadline = Instant::now() + patience;
            loop {
                let slot = match wire {
                    Wire::Control => &mut control,
                    Wire::Net => &mut net,
                };
                if let Some(socket) = slot.take() {
                    return Ok(socket);
                }
                match events_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(Ready::Connected(wire, socket)) => match wire {
                        Wire::Control => control = Some(socket),
                        Wire::Net => net = Some(socket),
                    },
                    Ok(Ready::Started(Ok(()))) => {}
                    Ok(Ready::Started(Err(error))) => return Err(Missed::Failed(error)),
                    Err(RecvTimeoutError::Timeout) => {
                        return Err(Missed::Timeout(
                            "the kernel may have panicked before its init could run".into(),
                        ));
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err(Missed::Failed(Error::backend(
                            HYPERVISOR,
                            "the machine's readiness channel closed before the guest connected",
                        )));
                    }
                }
            }
        },
        || Some(GuestConsole::Unkept),
    )
}

fn wait_for_stop(queue: &DispatchQueue, machine: &VZVirtualMachine, commands: &Receiver<Command>) {
    let answer = loop {
        match commands.recv() {
            Ok(Command::Stop(answer)) => break Some(answer),
            Ok(Command::Connect { port, reply }) => {
                let _ = reply.send(dial(queue, machine, port));
            }
            Err(_) => break None,
        }
    };
    let result = stop_gracefully(queue, machine);
    if let Some(answer) = answer {
        let _ = answer.send(result);
    }
}

/// # Errors
/// Returns [`Error::ForcedStop`] or [`Error::Backend`] if a graceful stop fails.
fn stop_gracefully(queue: &DispatchQueue, machine: &VZVirtualMachine) -> Result<(), Error> {
    stop_within(
        HYPERVISOR,
        || request_stop(queue, machine),
        || match read_state(queue, machine) {
            VZVirtualMachineState::Stopped => Ok(true),
            VZVirtualMachineState::Error => Err(Error::backend(
                HYPERVISOR,
                "the machine stopped in an error state",
            )),
            _ => Ok(false),
        },
        || force_stop(queue, machine),
    )
}

/// A Send handle to the worker that owns every !Send VZ object.
pub struct Guest {
    control: Option<Control>,
}

struct Control {
    cmd_tx: Sender<Command>,
    thread: JoinHandle<()>,
}

impl Guest {
    /// Stop and join once; shutdown takes the control before Drop.
    fn stop(&mut self) -> Result<(), Error> {
        let Some(control) = self.control.take() else {
            return Ok(());
        };
        let (answer_tx, answer_rx) = mpsc::sync_channel::<Result<(), Error>>(1);
        // A failed send still joins the worker; an exited worker has no machine left to force.
        let result = control
            .cmd_tx
            .send(Command::Stop(answer_tx))
            .map_or(Ok(()), |()| answer_rx.recv().unwrap_or(Ok(())));
        let _ = control.thread.join();
        result
    }
}

impl Machine for Guest {
    fn dial_engine(&self) -> std::io::Result<Socket> {
        self.connect_guest(ports::FACTORY)
    }

    /// # Errors
    /// Returns the channel, guest-dial, timeout or descriptor-duplication error.
    fn connect_guest(&self, port: u32) -> std::io::Result<Socket> {
        let gone = || {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the machine thread stopped before it could dial the guest",
            )
        };
        let control = self.control.as_ref().ok_or_else(gone)?;
        let (reply_tx, reply_rx) = mpsc::sync_channel(0);
        control
            .cmd_tx
            .send(Command::Connect {
                port,
                reply: reply_tx,
            })
            .map_err(|_| gone())?;
        reply_rx.recv().map_err(|_| gone())?
    }

    fn console(&self) -> GuestConsole {
        GuestConsole::Unkept
    }

    /// # Errors
    /// Returns [`Error::ForcedStop`] if the guest did not stop within the grace period.
    fn shutdown(mut self: Box<Self>) -> Result<(), Error> {
        self.stop()
    }
}

impl Drop for Guest {
    /// Dropping a machine stops it too, so a caller that forgets — or unwinds
    /// — does not leave a guest running.
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
