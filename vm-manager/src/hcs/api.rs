//! HCS is loaded on demand so missing Windows features yield refusals instead of loader failure.
//! An accepted asynchronous request still needs its operation result.
//! Handles have no thread affinity; DLL references outlive every function pointer.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{FARPROC, HMODULE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS, FormatMessageW,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::core::{HRESULT, PCWSTR, PWSTR};

use crate::error::Error;
use crate::hcs::win::{self, HYPERVISOR};

pub(super) type HcsSystem = *mut c_void;

pub(super) type HcsOperation = *mut c_void;

/// This code selects the group-membership remedy rather than a generic backend complaint.
pub(super) const HCS_E_ACCESS_DENIED: HRESULT = 0x8037_011B_u32.cast_signed();

/// A cold compute service may need a minute to start its worker.
const CALL_TIMEOUT_MS: u32 = 60_000;

const MESSAGE_BUFFER: u32 = 512;

/// Access-grant calls are optional; missing computestorage does not hide computecore availability.
pub(super) struct Api {
    create_operation: unsafe extern "system" fn(*const c_void, *const c_void) -> HcsOperation,
    close_operation: unsafe extern "system" fn(HcsOperation),
    wait_result: unsafe extern "system" fn(HcsOperation, u32, *mut PWSTR) -> HRESULT,
    create_system: unsafe extern "system" fn(
        PCWSTR,
        PCWSTR,
        HcsOperation,
        *const c_void,
        *mut HcsSystem,
    ) -> HRESULT,
    start_system: unsafe extern "system" fn(HcsSystem, HcsOperation, PCWSTR) -> HRESULT,
    shutdown_system: unsafe extern "system" fn(HcsSystem, HcsOperation, PCWSTR) -> HRESULT,
    terminate_system: unsafe extern "system" fn(HcsSystem, HcsOperation, PCWSTR) -> HRESULT,
    system_properties: unsafe extern "system" fn(HcsSystem, HcsOperation, PCWSTR) -> HRESULT,
    close_system: unsafe extern "system" fn(HcsSystem) -> HRESULT,
    service_properties: unsafe extern "system" fn(PCWSTR, *mut PWSTR) -> HRESULT,
    grant_vm_access: Option<unsafe extern "system" fn(PCWSTR, PCWSTR) -> HRESULT>,
    revoke_vm_access: Option<unsafe extern "system" fn(PCWSTR, PCWSTR) -> HRESULT>,
}

// SAFETY: immutable pointers reference DLLs never unloaded; HCS has no thread affinity.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

impl Api {
    /// Both successful resolution and refusal are cached for the process lifetime.
    pub(super) fn open() -> Result<&'static Self, Error> {
        static API: OnceLock<Result<Api, Error>> = OnceLock::new();
        API.get_or_init(Self::resolve)
            .as_ref()
            .map_err(Clone::clone)
    }

    fn resolve() -> Result<Self, Error> {
        let core = library("computecore.dll").ok_or_else(|| {
            Error::backend(
                HYPERVISOR,
                "this computer has no computecore.dll, the Windows component that creates \
                 virtual machines — the Virtual Machine Platform feature is not installed",
            )
        })?;
        // Windows 11 exports these in computecore; documentation names computestorage, so probe both.
        let storage = library("computestorage.dll");
        let in_either = |name: &std::ffi::CStr| {
            bind(core, name)
                .ok()
                .or_else(|| storage.and_then(|dll| bind(dll, name).ok()))
        };

        Ok(Self {
            create_operation: bind(core, c"HcsCreateOperation")?,
            close_operation: bind(core, c"HcsCloseOperation")?,
            wait_result: bind(core, c"HcsWaitForOperationResult")?,
            create_system: bind(core, c"HcsCreateComputeSystem")?,
            start_system: bind(core, c"HcsStartComputeSystem")?,
            shutdown_system: bind(core, c"HcsShutDownComputeSystem")?,
            terminate_system: bind(core, c"HcsTerminateComputeSystem")?,
            system_properties: bind(core, c"HcsGetComputeSystemProperties")?,
            close_system: bind(core, c"HcsCloseComputeSystem")?,
            service_properties: bind(core, c"HcsGetServiceProperties")?,
            grant_vm_access: in_either(c"HcsGrantVmAccess"),
            revoke_vm_access: in_either(c"HcsRevokeVmAccess"),
        })
    }

    /// # Errors
    /// Returns the HCS access-denial code or the service failure.
    pub(super) fn probe_service(&self) -> Result<(), HcsError> {
        let query = win::wide(r#"{"PropertyTypes":["Basic"]}"#.as_ref());
        let mut result: PWSTR = std::ptr::null_mut();
        // SAFETY: query is NUL-terminated and live; result receives an optional LocalAlloc document.
        let hr = unsafe { (self.service_properties)(query.as_ptr(), &raw mut result) };
        // SAFETY: HCS transfers this optional LocalAlloc result document.
        let document = unsafe { win::take_wide(result) };
        if hr < 0 {
            return Err(HcsError::new(hr, document.as_deref()));
        }
        Ok(())
    }

    /// # Errors
    /// Returns the service refusal, including configuration, file or access denial.
    pub(super) fn create(&self, id: &str, configuration: &str) -> Result<HcsSystem, HcsError> {
        let id = win::wide(id.as_ref());
        let configuration = win::wide(configuration.as_ref());
        let operation = self.operation()?;
        let mut system: HcsSystem = std::ptr::null_mut();
        // SAFETY: strings and operation are live; system is writable; null security uses the creating user.
        let hr = unsafe {
            (self.create_system)(
                id.as_ptr(),
                configuration.as_ptr(),
                operation.handle,
                std::ptr::null(),
                &raw mut system,
            )
        };
        self.settle(&operation, hr)?;
        if system.is_null() {
            return Err(HcsError::sentence(
                "the compute service reported success but handed back no machine",
            ));
        }
        Ok(system)
    }

    /// # Errors
    /// Returns the service refusal; a started machine has not yet proved guest readiness.
    pub(super) fn start(&self, system: HcsSystem) -> Result<(), HcsError> {
        self.act(system, self.start_system)
    }

    /// # Errors
    /// Returns the service refusal, including an already stopped machine treated as success upstream.
    pub(super) fn shutdown(&self, system: HcsSystem) -> Result<(), HcsError> {
        self.act(system, self.shutdown_system)
    }

    /// # Errors
    /// Returns the service refusal.
    pub(super) fn terminate(&self, system: HcsSystem) -> Result<(), HcsError> {
        self.act(system, self.terminate_system)
    }

    /// # Errors
    /// Returns the service refusal; an already absent machine is treated as stopped upstream.
    pub(super) fn state(&self, system: HcsSystem) -> Result<Option<String>, HcsError> {
        let query = win::wide(r#"{"PropertyTypes":["Basic"]}"#.as_ref());
        let operation = self.operation()?;
        // SAFETY: `system` is a live handle, `operation` one this function owns,
        // and `query` a NUL-terminated wide string alive for the call.
        let hr = unsafe { (self.system_properties)(system, operation.handle, query.as_ptr()) };
        let document = self.settle_with_document(&operation, hr)?;
        Ok(document
            .as_deref()
            .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
            .and_then(|properties| {
                properties
                    .get("State")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            }))
    }

    /// Closing the last handle terminates a running machine under its configured policy.
    pub(super) fn close(&self, system: HcsSystem) {
        if !system.is_null() {
            // SAFETY: `system` is a handle from `create` that no other thread
            // holds — `Guest::stop` takes it out of its slot before closing.
            let _ = unsafe { (self.close_system)(system) };
        }
    }

    /// # Errors
    /// Returns a failed access grant; an absent optional call is `Ok(false)`.
    pub(super) fn grant_vm_access(
        &self,
        id: &str,
        path: &std::path::Path,
    ) -> Result<bool, HcsError> {
        let Some(grant) = self.grant_vm_access else {
            return Ok(false);
        };
        let id = win::wide(id.as_ref());
        let path = win::wide(path.as_os_str());
        // SAFETY: both wide strings are NUL-terminated and outlive the call.
        let hr = unsafe { grant(id.as_ptr(), path.as_ptr()) };
        if hr < 0 {
            return Err(HcsError::new(hr, None));
        }
        Ok(true)
    }

    /// Revocation is best-effort: a leftover entry names a per-machine identity that never returns.
    pub(super) fn revoke_vm_access(&self, id: &str, path: &std::path::Path) {
        let Some(revoke) = self.revoke_vm_access else {
            return;
        };
        let id = win::wide(id.as_ref());
        let path = win::wide(path.as_os_str());
        // SAFETY: both wide strings are NUL-terminated and outlive the call.
        let _ = unsafe { revoke(id.as_ptr(), path.as_ptr()) };
    }

    fn act(
        &self,
        system: HcsSystem,
        call: unsafe extern "system" fn(HcsSystem, HcsOperation, PCWSTR) -> HRESULT,
    ) -> Result<(), HcsError> {
        let operation = self.operation()?;
        // SAFETY: `system` is a live handle, `operation` one this function
        // owns, and a null options string asks for the call's defaults.
        let hr = unsafe { call(system, operation.handle, std::ptr::null()) };
        self.settle(&operation, hr)
    }

    fn operation(&self) -> Result<Operation<'_>, HcsError> {
        // SAFETY: null optional context and callback request an operation for synchronous waiting.
        let handle = unsafe { (self.create_operation)(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(HcsError::sentence(
                "the compute service would not open an operation to track the request",
            ));
        }
        Ok(Operation { api: self, handle })
    }

    /// A rejected request is final; an accepted one still requires the operation result.
    fn settle(&self, operation: &Operation<'_>, accepted: HRESULT) -> Result<(), HcsError> {
        self.settle_with_document(operation, accepted).map(drop)
    }

    fn settle_with_document(
        &self,
        operation: &Operation<'_>,
        accepted: HRESULT,
    ) -> Result<Option<String>, HcsError> {
        if accepted < 0 {
            // Even a rejected request may have left an explanatory document on
            // the operation, so it is drained rather than discarded.
            let document = self.result_of(operation);
            return Err(HcsError::new(accepted, document.as_deref()));
        }
        let mut result: PWSTR = std::ptr::null_mut();
        // SAFETY: operation is live; result receives an optional LocalAlloc document.
        let hr = unsafe { (self.wait_result)(operation.handle, CALL_TIMEOUT_MS, &raw mut result) };
        // SAFETY: HCS transfers this optional LocalAlloc result document.
        let document = unsafe { win::take_wide(result) };
        if hr < 0 {
            return Err(HcsError::new(hr, document.as_deref()));
        }
        Ok(document)
    }

    /// Draining an operation enriches an already decided error; its result code is ignored.
    fn result_of(&self, operation: &Operation<'_>) -> Option<String> {
        let mut result: PWSTR = std::ptr::null_mut();
        // SAFETY: as in `settle`; a zero timeout asks for whatever is already
        // there without waiting.
        let _ = unsafe { (self.wait_result)(operation.handle, 0, &raw mut result) };
        // SAFETY: HCS transfers this optional LocalAlloc result document.
        unsafe { win::take_wide(result) }
    }
}

struct Operation<'a> {
    api: &'a Api,
    handle: HcsOperation,
}

impl Drop for Operation<'_> {
    fn drop(&mut self) {
        // SAFETY: `handle` came from `HcsCreateOperation`, is non-null (checked
        // at mint), and is closed exactly once — here.
        unsafe { (self.api.close_operation)(self.handle) };
    }
}

/// The code stays beside the text because access denial selects a distinct remedy.
#[derive(Debug)]
pub(super) struct HcsError {
    pub(super) code: HRESULT,
    text: String,
}

impl HcsError {
    fn new(code: HRESULT, document: Option<&str>) -> Self {
        let mut text = message_for(code);
        if let Some(detail) = document.map(str::trim).filter(|d| !d.is_empty()) {
            text.push_str(" (");
            text.push_str(detail);
            text.push(')');
        }
        Self { code, text }
    }

    /// `S_OK` prevents a synthetic complaint from being classified as an HCS failure code.
    fn sentence(text: &str) -> Self {
        Self {
            code: 0,
            text: text.to_string(),
        }
    }
}

impl std::fmt::Display for HcsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

fn library(name: &str) -> Option<HMODULE> {
    let name = win::wide(name.as_ref());
    // SAFETY: name is NUL-terminated; the DLL is never unloaded while its 'static pointers exist.
    let module = unsafe { LoadLibraryW(name.as_ptr()) };
    (!module.is_null()).then_some(module)
}

fn bind<T>(module: HMODULE, name: &std::ffi::CStr) -> Result<T, Error> {
    // SAFETY: `module` is a live module handle and `name` is NUL-terminated.
    let symbol: FARPROC = unsafe { GetProcAddress(module, name.as_ptr().cast()) };
    let symbol = symbol.ok_or_else(|| {
        Error::backend(
            HYPERVISOR,
            format!(
                "this computer's Host Compute System library has no {} — its Virtual Machine \
             Platform is too old for synod",
                name.to_string_lossy()
            ),
        )
    })?;
    // SAFETY: each name is paired with its computecore.h signature; T and FARPROC are function pointers.
    Ok(unsafe { std::mem::transmute_copy::<FARPROC, T>(&Some(symbol)) })
}

fn message_for(code: HRESULT) -> String {
    let mut buffer = [0u16; MESSAGE_BUFFER as usize];
    // SAFETY: buffer is writable for the supplied length; IGNORE_INSERTS permits a null argument array.
    let len = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            std::ptr::null(),
            code.cast_unsigned(),
            0,
            buffer.as_mut_ptr(),
            MESSAGE_BUFFER,
            std::ptr::null(),
        )
    };
    if len == 0 {
        return format!(
            "the compute service failed with error 0x{:08X}",
            code.cast_unsigned()
        );
    }
    let text = String::from_utf16_lossy(&buffer[..len as usize]);
    let text = text.trim().to_string();
    if text.is_empty() {
        format!(
            "the compute service failed with error 0x{:08X}",
            code.cast_unsigned()
        )
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_api_either_resolves_or_names_what_is_missing() {
        match Api::open() {
            Ok(_) => {}
            Err(Error::Backend { why, .. }) => assert!(
                why.contains("Virtual Machine Platform"),
                "a refusal must name the feature: {why}"
            ),
            Err(other) => panic!("a refusal must name the feature: {other:?}"),
        }
    }

    #[test]
    fn windows_explains_the_access_denial_itself() {
        let text = message_for(HCS_E_ACCESS_DENIED);
        assert!(
            text.contains("Hyper-V Administrators"),
            "expected Windows' own words about the group, got: {text}"
        );
    }

    #[test]
    fn an_unexplained_code_still_renders() {
        let text = message_for(0x8037_0FFF_u32.cast_signed());
        assert_ne!(text.trim(), "");
    }
}
