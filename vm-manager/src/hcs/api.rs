//! The Host Compute System API, loaded rather than linked.
//!
//! HCS is the interface Windows offers for creating a virtual machine without
//! going through Hyper-V Manager: `computecore.dll` is what WSL 2 and Docker's
//! Linux containers are built on, and it is synod's Windows route for exactly
//! that reason — it is the road with traffic on it.
//!
//! # Why every entry point is resolved at runtime
//!
//! There is no Rust binding to HCS, so these declarations are the binding.
//! They could have been `#[link(name = "computecore")]` externs, which would
//! be shorter — and wrong: a static import makes the *process* fail to start
//! on a Windows without the Virtual Machine Platform feature, before any code
//! of ours can explain why.  Synod is a desktop application whose whole
//! Windows story includes running on a machine that cannot host a guest, and
//! saying so plainly ([`crate::detect`]).  So the library is loaded on demand
//! and a missing DLL is data — [`Api::open`]'s `Err` — rather than a launch
//! failure.
//!
//! # The operation protocol
//!
//! Nearly every HCS call is asynchronous in the same shape: the caller mints
//! an *operation*, hands it to the call, and then waits on the operation for
//! the real result.  The synchronous-looking `HRESULT` a call returns says
//! only whether the request was *accepted*; the outcome — including access
//! denials and configuration rejections — arrives from
//! `HcsWaitForOperationResult`, together with an optional JSON *result
//! document* carrying the service's own error text.  [`Operation`] owns one
//! for the length of one call, and [`Api::settle`] is the whole protocol in
//! one place, so no caller can forget half of it.
//!
//! # Thread affinity
//!
//! There is none.  Unlike Virtualization.framework — whose `VZVirtualMachine`
//! must be touched only from the one serial queue it was born on, which is why
//! `vm-manager/src/vz.rs` gives each machine a thread and speaks to it by message — an
//! HCS system is a handle, valid from any thread, with no callback queue to
//! own.  That is why this backend has no worker thread: the handle is simply
//! held, and the two threads that exist ([`super::console`]'s pump and the
//! control-plane accept) are there to serve blocking I/O, not the API.

use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{FARPROC, HMODULE, LocalFree};
use windows_sys::Win32::System::Diagnostics::Debug::{
    FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS, FormatMessageW,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::core::{HRESULT, PCWSTR, PWSTR};

/// A live compute system — one virtual machine — as HCS names it.
pub(super) type HcsSystem = *mut c_void;

/// One in-flight HCS call's operation handle.
pub(super) type HcsOperation = *mut c_void;

/// `HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED)`-adjacent, but its own code: HCS's
/// way of saying the caller is neither an administrator nor a member of the
/// *Hyper-V Administrators* group, and so may not touch virtual machines at
/// all.  It is the one HCS failure synod must recognise rather than merely
/// report, because it is the one an IT department fixes with a group
/// membership rather than a bug report ([`super::NOT_PERMITTED`]).
pub(super) const HCS_E_ACCESS_DENIED: HRESULT = 0x8037_011B_u32.cast_signed();

/// How long any single HCS call is given to settle before the backend stops
/// waiting on it.  Generous: it spans the service starting a VM worker
/// process, and a cold `vmcompute` on a loaded laptop is not quick.
const CALL_TIMEOUT_MS: u32 = 60_000;

/// How much of Windows' own explanation of an error code is kept.  System
/// messages are one or two sentences; this is several times that.
const MESSAGE_BUFFER: u32 = 512;

/// The entry points this backend uses, resolved once per process.
///
/// `computestorage.dll` is a second library and a soft dependency: it carries
/// only `HcsGrantVmAccess`, so a Windows that has `computecore.dll` but not it
/// can still be reported precisely rather than as one undifferentiated
/// absence.
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

// SAFETY: `Api` holds function pointers into two DLLs that are never unloaded
// (the process keeps its `LoadLibraryW` reference for its whole life), and HCS
// itself has no thread affinity — see the module docs.  There is no interior
// mutability here at all: every field is set once by `open`.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

impl Api {
    /// The process's one resolved copy of the API, or the reason this Windows
    /// cannot offer it.
    ///
    /// Resolution is attempted once and its outcome — success or refusal — is
    /// remembered, so a machine without the feature is not re-probed on every
    /// [`crate::detect`].
    pub(super) fn open() -> Result<&'static Self, String> {
        static API: OnceLock<Result<Api, String>> = OnceLock::new();
        API.get_or_init(Self::resolve)
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Load `computecore.dll` and bind every entry point, or say what is
    /// missing.
    fn resolve() -> Result<Self, String> {
        let core = library("computecore.dll").ok_or_else(|| {
            "this computer has no computecore.dll, the Windows component that creates virtual \
             machines — the Virtual Machine Platform feature is not installed"
                .to_string()
        })?;
        // The two access calls are looked for in `computecore.dll` first and
        // `computestorage.dll` second, because that is where they actually are.
        // Microsoft's own Go binding names them `computestorage.HcsGrantVmAccess`
        // and the documentation puts them in that library, but on Windows 11
        // (10.0.26100) `computestorage.dll` exports neither: both live in
        // `computecore.dll`, beside every other `Hcs*` entry point. Rather than
        // trust either answer, both libraries are asked — which also keeps a
        // build where the documentation *is* right working.
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

    /// Ask the compute service about itself — the cheapest call that still
    /// crosses the access check, and so the one [`crate::detect`] uses to find
    /// out whether this user may touch virtual machines *before* a session is
    /// under way and a folder has been granted.
    ///
    /// # Errors
    /// Returns the call's `HRESULT` — [`HCS_E_ACCESS_DENIED`] when the caller
    /// is outside the Hyper-V Administrators group, or another code when the
    /// service is not running.
    pub(super) fn probe_service(&self) -> Result<(), HcsError> {
        let query = wide(r#"{"PropertyTypes":["Basic"]}"#);
        let mut result: PWSTR = std::ptr::null_mut();
        // SAFETY: `query` is a NUL-terminated wide string alive for the call,
        // and `result` is a writable slot the service either fills with a
        // `LocalAlloc`'d document or leaves null.
        let hr = unsafe { (self.service_properties)(query.as_ptr(), &raw mut result) };
        let document = take_document(result);
        if hr < 0 {
            return Err(HcsError::new(hr, document.as_deref()));
        }
        Ok(())
    }

    /// Create a compute system from its JSON `configuration`, and wait for the
    /// service to say whether it took.
    ///
    /// The returned handle owns the VM: with
    /// `ShouldTerminateOnLastHandleClosed` set in the configuration (as
    /// [`super::spec`] does), losing it is how a crashed synod still leaves no
    /// guest running.
    ///
    /// # Errors
    /// Returns the service's own refusal — an unusable configuration, a file
    /// it cannot open, or [`HCS_E_ACCESS_DENIED`].
    pub(super) fn create(&self, id: &str, configuration: &str) -> Result<HcsSystem, HcsError> {
        let id = wide(id);
        let configuration = wide(configuration);
        let operation = self.operation()?;
        let mut system: HcsSystem = std::ptr::null_mut();
        // SAFETY: both wide strings outlive the call; `operation` is a live
        // operation handle this function owns; the security-descriptor
        // argument is optional and null asks for the service's default, which
        // grants the creating user; `system` is a writable slot.
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

    /// Start a created system and wait for it to be running.
    ///
    /// "Running" here means the machine is executing, not that its guest has
    /// booted: the kernel may still panic on the way up.  Proof of a live
    /// guest is the control-plane connection, which is why
    /// [`Hypervisor::boot`](crate::Hypervisor::boot) waits for that too.
    ///
    /// # Errors
    /// Returns the service's own refusal.
    pub(super) fn start(&self, system: HcsSystem) -> Result<(), HcsError> {
        self.act(system, self.start_system)
    }

    /// Ask the guest to shut itself down, and wait for the machine to stop.
    ///
    /// # Errors
    /// Returns the service's own refusal — including the ordinary case of a
    /// machine that has already stopped on its own, which the caller treats as
    /// success rather than failure.
    pub(super) fn shutdown(&self, system: HcsSystem) -> Result<(), HcsError> {
        self.act(system, self.shutdown_system)
    }

    /// Stop the machine without asking the guest.
    ///
    /// # Errors
    /// Returns the service's own refusal.
    pub(super) fn terminate(&self, system: HcsSystem) -> Result<(), HcsError> {
        self.act(system, self.terminate_system)
    }

    /// What state the machine is in — `"Running"`, `"Stopped"`, and the rest of
    /// the service's own vocabulary — or `None` when it left no state in its
    /// answer.
    ///
    /// This is how [`super::Guest::stop`] tells a guest that powered itself off
    /// from one that hung, which is the difference [`crate::Machine::shutdown`]
    /// is contracted to report.  The `Basic` property set is the cheapest that
    /// carries it.
    ///
    /// # Errors
    /// Returns the service's refusal — including the ordinary case of a machine
    /// that has already gone, which a caller reads as *stopped* rather than as
    /// a fault.
    pub(super) fn state(&self, system: HcsSystem) -> Result<Option<String>, HcsError> {
        let query = wide(r#"{"PropertyTypes":["Basic"]}"#);
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

    /// Release the handle to a system.  Under
    /// `ShouldTerminateOnLastHandleClosed` this also stops a machine still
    /// running, which is the safety net rather than the intended path.
    pub(super) fn close(&self, system: HcsSystem) {
        if !system.is_null() {
            // SAFETY: `system` is a handle from `create` that no other thread
            // holds — `Guest::stop` takes it out of its slot before closing.
            let _ = unsafe { (self.close_system)(system) };
        }
    }

    /// Give one virtual machine read/write access to one file.
    ///
    /// A VM's worker process runs as its own virtual account, not as the user
    /// who created it, so a boot artifact sitting in that user's cache is
    /// unreadable to the machine until this is called on it.  Wanting it is
    /// what tells you the files really are opened by the hypervisor rather
    /// than read by synod and passed along.
    ///
    /// Answers `Ok(false)` when this Windows offers no such call at all, which
    /// is not the same as failing: the grant is an *enabling* step, and a
    /// platform without it may still open the files perfectly well. Refusing a
    /// session over a missing optional call would turn a maybe into a no, so
    /// the caller notes the absence and lets the boot speak for itself.
    ///
    /// # Errors
    /// Returns the refusal when the call exists and failed — which, unlike its
    /// absence, really does mean the machine will not be able to read the file.
    pub(super) fn grant_vm_access(
        &self,
        id: &str,
        path: &std::path::Path,
    ) -> Result<bool, HcsError> {
        let Some(grant) = self.grant_vm_access else {
            return Ok(false);
        };
        let id = wide(id);
        let path = wide_path(path);
        // SAFETY: both wide strings are NUL-terminated and outlive the call.
        let hr = unsafe { grant(id.as_ptr(), path.as_ptr()) };
        if hr < 0 {
            return Err(HcsError::new(hr, None));
        }
        Ok(true)
    }

    /// Take a machine's access to one file away again.
    ///
    /// Best effort, and called only on teardown: a granted entry names a *per
    /// machine* identity that will never exist again, so leaving one behind is
    /// litter on somebody's disk rather than a hole. Revoking is how synod
    /// leaves the access-control state it found. Nothing is reported, because
    /// nothing can be done about a failure at this point in a machine's life.
    pub(super) fn revoke_vm_access(&self, id: &str, path: &std::path::Path) {
        let Some(revoke) = self.revoke_vm_access else {
            return;
        };
        let id = wide(id);
        let path = wide_path(path);
        // SAFETY: both wide strings are NUL-terminated and outlive the call.
        let _ = unsafe { revoke(id.as_ptr(), path.as_ptr()) };
    }

    /// The shared body of every `(system, operation, options)` call: mint an
    /// operation, make the call, wait for the real outcome.
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

    /// Mint one operation, or say that the service would not give one.
    fn operation(&self) -> Result<Operation<'_>, HcsError> {
        // SAFETY: both arguments are the optional context/callback pair, and
        // null for each asks for an operation nobody is called back about —
        // which is what a caller that waits synchronously wants.
        let handle = unsafe { (self.create_operation)(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(HcsError::sentence(
                "the compute service would not open an operation to track the request",
            ));
        }
        Ok(Operation { api: self, handle })
    }

    /// Turn a call's acceptance code plus its operation into one outcome.
    ///
    /// The asymmetry this hides is the whole reason it exists: a *failing*
    /// acceptance code is final, while a *successful* one says nothing — the
    /// outcome is still to be waited for, and an access denial arrives there.
    /// Every error carries the service's own result document when it left one.
    fn settle(&self, operation: &Operation<'_>, accepted: HRESULT) -> Result<(), HcsError> {
        self.settle_with_document(operation, accepted).map(drop)
    }

    /// [`Self::settle`], keeping the result document — which for most calls is
    /// nothing, and for a properties query is the answer itself.
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
        // SAFETY: `operation` is live for the call and `result` is a writable
        // slot the service either fills with a `LocalAlloc`'d document or
        // leaves null.
        let hr = unsafe { (self.wait_result)(operation.handle, CALL_TIMEOUT_MS, &raw mut result) };
        let document = take_document(result);
        if hr < 0 {
            return Err(HcsError::new(hr, document.as_deref()));
        }
        Ok(document)
    }

    /// Drain whatever document an operation carries, ignoring its code — used
    /// only to enrich an error already decided.
    fn result_of(&self, operation: &Operation<'_>) -> Option<String> {
        let mut result: PWSTR = std::ptr::null_mut();
        // SAFETY: as in `settle`; a zero timeout asks for whatever is already
        // there without waiting.
        let _ = unsafe { (self.wait_result)(operation.handle, 0, &raw mut result) };
        take_document(result)
    }
}

/// One operation handle, closed on every path out of the call that minted it.
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

/// Why HCS refused.
///
/// Keeps the `HRESULT` beside the rendered text because one code —
/// [`HCS_E_ACCESS_DENIED`] — is a *decision* upstream and not merely a message
/// to print: [`crate::detect`] turns it into the refusal that names the
/// Hyper-V Administrators group.
#[derive(Debug)]
pub(super) struct HcsError {
    pub(super) code: HRESULT,
    text: String,
}

impl HcsError {
    /// An error carrying Windows' own words for `code`, plus the service's
    /// result document when it left one.
    fn new(code: HRESULT, document: Option<&str>) -> Self {
        let mut text = message_for(code);
        if let Some(detail) = document.map(str::trim).filter(|d| !d.is_empty()) {
            text.push_str(" (");
            text.push_str(detail);
            text.push(')');
        }
        Self { code, text }
    }

    /// An error of synod's own making — an impossible answer from the service
    /// rather than a code it reported.  `S_OK` as the code is deliberate: there
    /// is no `HRESULT` behind it to classify, and nothing upstream may mistake
    /// it for one it recognises.
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

/// Load one DLL by name, or `None` if this Windows has not got it.
fn library(name: &str) -> Option<HMODULE> {
    let name = wide(name);
    // SAFETY: `name` is a NUL-terminated wide string alive for the call.  The
    // module reference this takes is deliberately never released: the API's
    // function pointers are `'static`, so the library must outlive them.
    let module = unsafe { LoadLibraryW(name.as_ptr()) };
    (!module.is_null()).then_some(module)
}

/// Resolve one entry point, or say which name was missing.
fn bind<T>(module: HMODULE, name: &std::ffi::CStr) -> Result<T, String> {
    // SAFETY: `module` is a live module handle and `name` is NUL-terminated.
    let symbol: FARPROC = unsafe { GetProcAddress(module, name.as_ptr().cast()) };
    let symbol = symbol.ok_or_else(|| {
        format!(
            "this computer's Host Compute System library has no {} — its Virtual Machine \
             Platform is too old for synod",
            name.to_string_lossy()
        )
    })?;
    // SAFETY: HCS entry points are `extern "system"` functions, and each call
    // site above pairs a name with the signature `computecore.h` declares for
    // it; `T` is always such a function pointer, of the same size as `FARPROC`.
    Ok(unsafe { std::mem::transmute_copy::<FARPROC, T>(&Some(symbol)) })
}

/// A NUL-terminated wide string, owned for as long as the call that reads it.
fn wide(text: &str) -> Vec<u16> {
    OsStr::new(text).encode_wide().chain(Some(0)).collect()
}

/// A NUL-terminated wide path, without the lossy round-trip through `str` that
/// a path with unpaired surrogates would not survive.
fn wide_path(path: &std::path::Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Adopt a `LocalAlloc`'d result document into an owned `String`, releasing the
/// service's allocation.
fn take_document(result: PWSTR) -> Option<String> {
    if result.is_null() {
        return None;
    }
    // SAFETY: HCS documents its result documents as NUL-terminated wide
    // strings allocated with `LocalAlloc`, transferred to the caller.  The
    // length is measured, the contents copied, and the allocation released
    // exactly once.
    let text = unsafe {
        let mut len = 0usize;
        while *result.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(result, len))
    };
    // SAFETY: `result` is the `LocalAlloc`'d block just read and never used
    // again.
    unsafe { LocalFree(result.cast()) };
    Some(text)
}

/// Windows' own sentence for an `HRESULT`, or the bare code when the system
/// has no words for it.
///
/// This is where the Hyper-V Administrators text comes from — Windows itself
/// explains that code, and better than a paraphrase would.
fn message_for(code: HRESULT) -> String {
    let mut buffer = [0u16; MESSAGE_BUFFER as usize];
    // SAFETY: `buffer` is a writable slice of the length passed; the
    // ignore-inserts flag means no argument array is read, so passing null for
    // it is correct.
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

    /// The API resolves on any Windows with the Virtual Machine Platform, and
    /// refuses by naming the feature otherwise.  Either answer is correct
    /// here — the law is that both are *answers*, since a static import would
    /// instead have failed the process before this test could run.
    #[test]
    fn the_api_either_resolves_or_names_what_is_missing() {
        match Api::open() {
            Ok(_) => {}
            Err(why) => assert!(
                why.contains("Virtual Machine Platform"),
                "a refusal must name the feature: {why}"
            ),
        }
    }

    /// Windows explains the one code synod classifies rather than merely
    /// prints, and its explanation names the group an IT department must add
    /// the user to.  If this ever stops holding, [`super::NOT_PERMITTED`] is
    /// carrying the whole message alone and should say more.
    #[test]
    fn windows_explains_the_access_denial_itself() {
        let text = message_for(HCS_E_ACCESS_DENIED);
        assert!(
            text.contains("Hyper-V Administrators"),
            "expected Windows' own words about the group, got: {text}"
        );
    }

    /// An unknown code still renders to something a person can act on, rather
    /// than an empty string.
    #[test]
    fn an_unexplained_code_still_renders() {
        let text = message_for(0x8037_0FFF_u32.cast_signed());
        assert!(!text.trim().is_empty());
    }
}
