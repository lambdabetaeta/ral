//! The two ways the broker is started on Windows.
//!
//! Everything interesting is in [`vm_manager::broker`] — this file is only the
//! two ways the same service can be started:
//!
//! - as a **Windows service**, which is how it runs on a user's computer. The
//!   installer registers it, Windows starts it at boot, and it must announce
//!   itself to the service controller within a few seconds or be killed for not
//!   answering.
//! - as an **ordinary console program** (`--console`), which is how it is
//!   developed. Identical behaviour, no service controller, and the guest's
//!   console output lands on a terminal somebody is watching.
//!
//! The second mode is not a lesser one: it is how a maintainer sees a boot fail.
//! A service's `stdout` goes nowhere, so the guest's own console — the thing
//! that says *why* a kernel did not come up — is invisible in the mode users
//! run. Run it in a console to watch a machine, and as a service to serve one.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::NO_ERROR;
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerW, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};

/// The name the service is registered under, and the one the controller
/// addresses it by. `synod/wix/broker-service.wxs` installs it under exactly
/// this name; the two must not drift.
const SERVICE_NAME: &str = "SynodMachineBroker";

pub fn start() {
    if std::env::args().any(|arg| arg == "--console") {
        println!("synod machine broker: serving {}", vm_manager::broker::PIPE);
        println!("boot media: {:?}", vm_manager::broker::service::media());
        if let Err(cause) = vm_manager::broker::service::serve() {
            eprintln!("synod machine broker: stopped listening: {cause}");
            std::process::exit(1);
        }
        return;
    }

    let mut name: Vec<u16> = SERVICE_NAME.encode_utf16().chain(Some(0)).collect();
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        // The table is terminated by a pair of nulls, not by a count.
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    // SAFETY: `table` is a null-terminated service table alive for the call, and
    // `name` outlives it.  This call does not return until the service stops —
    // or fails immediately when the program was not started by the service
    // controller, which is the case the message below explains.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        eprintln!(
            "synod machine broker: this program is a Windows service. Windows starts it; to run \
             it by hand — which is how you watch a guest boot — pass --console."
        );
        std::process::exit(1);
    }
}

/// The service's own entry point, called by the controller on its own thread.
///
/// The order is fixed by the platform: report `RUNNING` *first*, then serve.
/// A service that starts work before it reports is killed for not answering,
/// and the work here never returns on its own.
extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let name: Vec<u16> = SERVICE_NAME.encode_utf16().chain(Some(0)).collect();
    // SAFETY: `name` is a NUL-terminated wide string alive for the call.
    let status_handle = unsafe { RegisterServiceCtrlHandlerW(name.as_ptr(), Some(control)) };
    if status_handle.is_null() {
        return;
    }
    // Published for [`control`], which the platform calls with no context of its
    // own and so has nowhere else to read it from.
    STATUS.store(status_handle.cast(), std::sync::atomic::Ordering::SeqCst);

    report(status_handle, SERVICE_RUNNING, SERVICE_ACCEPT_STOP);
    let _ = vm_manager::broker::service::serve();
    report(status_handle, SERVICE_STOPPED, 0);
}

/// The registered status handle, so [`control`] — which the platform calls with
/// no context of its own — can answer with it.
static STATUS: std::sync::atomic::AtomicPtr<c_void> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// What the service does when the controller speaks to it.
///
/// A stop is honoured by exiting the process, and deliberately so: every machine
/// this broker owns is held by the thread serving its client, so process exit is
/// what tears them all down. Unwinding those threads politely would mean
/// tracking them, which would be a second bookkeeping of live machines beside
/// the one the connections already are.
///
/// The handler returns nothing — this is the original `HandlerFunction`, not the
/// `Ex` form that answers with a status code — so a control this service does
/// not implement is simply not acted on.
unsafe extern "system" fn control(code: u32) {
    if code != SERVICE_CONTROL_STOP && code != SERVICE_CONTROL_SHUTDOWN {
        return;
    }
    let handle: SERVICE_STATUS_HANDLE = STATUS.load(std::sync::atomic::Ordering::SeqCst).cast();
    if !handle.is_null() {
        report(handle, SERVICE_STOPPED, 0);
    }
    std::process::exit(0);
}

/// Tell the controller where the service is in its life.
fn report(handle: SERVICE_STATUS_HANDLE, state: u32, accepted: u32) {
    let mut status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: NO_ERROR,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    };
    // SAFETY: `handle` is a registered status handle and `status` a fully
    // initialised structure of the type the call reads.
    unsafe { SetServiceStatus(handle, &raw mut status) };
}
