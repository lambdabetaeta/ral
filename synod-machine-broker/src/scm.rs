//! `--console` runs the same broker with the guest's console visible on a terminal.

#![allow(
    clippy::disallowed_macros,
    reason = "[silent:windows-host] a service diagnostic with no ral shell to print through"
)]
use std::ffi::c_void;

use vm_manager::hcs::win::wide;

use crate::site::{Silent, site};
use windows_sys::Win32::Foundation::NO_ERROR;
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerW, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};

/// Must match the service name in `synod/wix/broker-service.wxs`.
const SERVICE_NAME: &str = "SynodMachineBroker";

pub(crate) fn start() {
    if std::env::args().any(|arg| arg == "--console") {
        println!(
            "synod machine broker: serving {}",
            vm_manager::broker::wire::PIPE
        );
        let media = std::env::current_exe()
            .map_err(|cause| vm_manager::Error::ServiceUnlocated {
                cause: cause.to_string(),
            })
            .and_then(|exe| vm_manager::media::locate(&exe))
            .map(vm_manager::media::Layout::found);
        println!("boot media: {media:?}");
        if let Err(cause) = crate::service::serve() {
            eprintln!("synod machine broker: stopped listening: {cause}");
            site!(Silent::ConsoleListenerStopped, std::process::exit(1));
        }
        return;
    }

    let mut name: Vec<u16> = wide(SERVICE_NAME.as_ref());
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
    // SAFETY: `table` is null-terminated and `name` outlives the blocking call.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        eprintln!(
            "synod machine broker: this program is a Windows service. Windows starts it; to run \
             it by hand — which is how you watch a guest boot — pass --console."
        );
        site!(Silent::DispatcherRefused, std::process::exit(1));
    }
}

/// Report `RUNNING` before serving so the controller does not kill an unresponsive service.
extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let name: Vec<u16> = wide(SERVICE_NAME.as_ref());
    // SAFETY: `name` is a NUL-terminated wide string alive for the call.
    let status_handle = unsafe { RegisterServiceCtrlHandlerW(name.as_ptr(), Some(control)) };
    if status_handle.is_null() {
        return;
    }
    STATUS.store(status_handle.cast(), std::sync::atomic::Ordering::SeqCst);

    report(status_handle, SERVICE_RUNNING, SERVICE_ACCEPT_STOP);
    let _ = crate::service::serve();
    report(status_handle, SERVICE_STOPPED, 0);
}

/// [`control`] receives no context, so its status handle is published here.
static STATUS: std::sync::atomic::AtomicPtr<c_void> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Process exit tears down client-owned machines without a second registry.
unsafe extern "system" fn control(code: u32) {
    if code != SERVICE_CONTROL_STOP && code != SERVICE_CONTROL_SHUTDOWN {
        return;
    }
    let handle: SERVICE_STATUS_HANDLE = STATUS.load(std::sync::atomic::Ordering::SeqCst).cast();
    if !handle.is_null() {
        report(handle, SERVICE_STOPPED, 0);
    }
    site!(Silent::ServiceStop, std::process::exit(0));
}

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
