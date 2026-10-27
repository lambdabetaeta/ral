use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use block2::{DynBlock, RcBlock};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSObject, NSObjectProtocol};
use objc2_virtualization::{
    VZVirtioSocketConnection, VZVirtioSocketDevice, VZVirtioSocketListener,
    VZVirtioSocketListenerDelegate, VZVirtualMachine, VZVirtualMachineState,
};

use crate::error::{Error, Wire};
use crate::site::{Silent, site};

pub(super) enum Ready {
    Started(Result<(), Error>),
    Connected(Wire, OwnedFd),
}

const GUEST_DIAL_PATIENCE: Duration = Duration::from_secs(5);

pub(super) fn dial(
    queue: &DispatchQueue,
    machine: &VZVirtualMachine,
    port: u32,
) -> std::io::Result<OwnedFd> {
    use std::io::{Error as IoError, ErrorKind};

    let machine_ptr = core::ptr::from_ref(machine);
    let (done_tx, done_rx) = std::sync::mpsc::channel::<std::io::Result<OwnedFd>>();

    on_queue(queue, move || {
        let done_tx = done_tx.clone();
        // SAFETY: on the machine's queue; the framework retains the connect completion block.
        unsafe {
            let machine = &*machine_ptr;
            let devices = machine.socketDevices();
            let Some(device) = devices.firstObject() else {
                let _ = done_tx.send(Err(IoError::new(
                    ErrorKind::ConnectionRefused,
                    "the machine has no virtio socket device",
                )));
                return;
            };
            let Ok(socket) = device.downcast::<VZVirtioSocketDevice>() else {
                let _ = done_tx.send(Err(IoError::new(
                    ErrorKind::ConnectionRefused,
                    "the machine's socket device is not virtio",
                )));
                return;
            };
            let completion = RcBlock::new(
                move |connection: *mut VZVirtioSocketConnection,
                      error: *mut objc2_foundation::NSError| {
                    let outcome = if let Some(connection) = connection.as_ref() {
                        BorrowedFd::borrow_raw(connection.fileDescriptor()).try_clone_to_owned()
                    } else if let Some(error) = error.as_ref() {
                        Err(IoError::new(
                            ErrorKind::ConnectionRefused,
                            format!(
                                "the guest refused the dial: {}",
                                error.localizedDescription()
                            ),
                        ))
                    } else {
                        Err(IoError::new(
                            ErrorKind::ConnectionRefused,
                            "the connect completed with neither a connection nor an error",
                        ))
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
            Err(IoError::new(
                ErrorKind::TimedOut,
                format!(
                    "nothing in the guest answered a dial to port {port} within {}s — a guest \
                     with nothing bound there refuses within milliseconds, so a silence this \
                     long means the machine never delivered the dial at all",
                    GUEST_DIAL_PATIENCE.as_secs()
                ),
            ))
        })
}

pub(super) fn request_stop(queue: &DispatchQueue, machine: &VZVirtualMachine) {
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

pub(super) fn force_stop(queue: &DispatchQueue, machine: &VZVirtualMachine) {
    let machine_ptr = core::ptr::from_ref(machine);
    on_queue(queue, move || {
        // SAFETY: on the machine's queue; canStop guards the destructive stop.
        unsafe {
            let machine = &*machine_ptr;
            if machine.canStop() {
                let sink = RcBlock::new(|_error: *mut objc2_foundation::NSError| {});
                machine.stopWithCompletionHandler(&sink);
            }
        }
    });
}

pub(super) fn read_state(
    queue: &DispatchQueue,
    machine: &VZVirtualMachine,
) -> VZVirtualMachineState {
    let machine_ptr = core::ptr::from_ref(machine);
    let slot = AtomicIsize::new(VZVirtualMachineState::Stopped.0);
    let slot_ptr = core::ptr::from_ref(&slot);
    on_queue(queue, move || {
        // SAFETY: the queue call synchronously borrows the machine and atomic result slot.
        let state = unsafe { (*machine_ptr).state() };
        unsafe { &*slot_ptr }.store(state.0, Ordering::SeqCst);
    });
    VZVirtualMachineState(slot.load(Ordering::SeqCst))
}

pub(super) fn on_queue(queue: &DispatchQueue, body: impl Fn() + 'static) {
    let block = RcBlock::new(body);
    let block: *mut DynBlock<dyn Fn()> = core::ptr::from_ref(&*block).cast_mut();
    // SAFETY: exec_sync invokes the live block once and does not retain it.
    unsafe { queue.exec_sync_with_block(block) };
}

pub(super) fn new_listener(
    readiness: &Retained<SocketReadiness>,
) -> Retained<VZVirtioSocketListener> {
    // SAFETY: constructs a listener and sets its (weak) delegate to the
    // readiness object, which the caller keeps alive.
    unsafe {
        let listener = VZVirtioSocketListener::new();
        listener.setDelegate(Some(ProtocolObject::from_ref(&**readiness)));
        listener
    }
}

pub(super) struct SocketReadinessIvars {
    wire: Wire,
    announce: Mutex<Option<Sender<Ready>>>,
}

define_class!(
    // SAFETY: NSObject adds no subclassing requirements; this class has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "RalVzSocketReadiness"]
    #[ivars = SocketReadinessIvars]
    pub(super) struct SocketReadiness;

    unsafe impl NSObjectProtocol for SocketReadiness {}

    unsafe impl VZVirtioSocketListenerDelegate for SocketReadiness {
        #[unsafe(method(listener:shouldAcceptNewConnection:fromSocketDevice:))]
        fn should_accept(
            &self,
            _listener: &VZVirtioSocketListener,
            connection: &VZVirtioSocketConnection,
            _device: &VZVirtioSocketDevice,
        ) -> bool {
            // SAFETY: duplicate the live borrowed descriptor before VZ can close its connection object.
            let dup = (unsafe { BorrowedFd::borrow_raw(connection.fileDescriptor()) })
                .try_clone_to_owned();

            // Only on a successful dup is the one-shot announce spent, and
            // only then is the connection accepted.
            if let Ok(control_socket) = dup
                && let Ok(mut slot) = site!(Silent::AnnounceLock, &self.ivars().announce => lock())
                && let Some(announce) = slot.take()
            {
                let _ = announce.send(Ready::Connected(self.ivars().wire, control_socket));
                true
            } else {
                false
            }
        }
    }
);

impl SocketReadiness {
    pub(super) fn new(wire: Wire, announce: Sender<Ready>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(SocketReadinessIvars {
            wire,
            announce: Mutex::new(Some(announce)),
        });
        // SAFETY: `super(this), init` is `NSObject`'s designated initializer
        // on a freshly allocated instance whose ivars are set.
        unsafe { msg_send![super(this), init] }
    }
}
