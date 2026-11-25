//! `OpenVMM` runs alone in an anonymous kill-on-close job, so it cannot outlive synod.
//! It is ready once its RPC socket accepts a connection; until then its exit says why not.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::net::TcpStream;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_PROCESS_ABORTED, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::afunix::{self, Address};
use super::win::{self, HYPERVISOR};
use crate::error::{Act, Error, HostFile};
use crate::lifecycle::{PULSE, RPC_SOCKET};
use crate::site::{Silent, site};

/// How long a process that failed us, or was ended, is given to show it has exited.
const DYING_GRACE: Duration = Duration::from_secs(2);

/// How much of the log's end a refusal quotes.
const TAIL_BYTES: u64 = 4096;
const TAIL_LINES: usize = 6;

/// Exactly one child, so ending the job ends `OpenVMM`.
pub(super) struct Vmm {
    job: OwnedHandle,
    process: OwnedHandle,
    log: Option<PathBuf>,
}

/// Starts `OpenVMM` in `dir`, inside its job from its first instruction.
///
/// # Errors
/// Returns the path, job or launch refusal.
pub(super) fn start(exe: &Path, dir: &Path, log: Option<(PathBuf, File)>) -> Result<Vmm, Error> {
    let (application, directory) = (win::plain(exe)?, win::plain(dir)?);
    let unstarted = |what: &str, cause: io::Error| {
        Error::backend(HYPERVISOR, format!("synod could not {what}: {cause}"))
    };
    let job = kill_on_close_job()
        .map_err(|cause| unstarted("make the job that ties OpenVMM to it", cause))?;
    let null = |access: &mut OpenOptions| {
        site!(Silent::VmmStdio, access => open("NUL"))
            .map(OwnedHandle::from)
            .map_err(|cause| unstarted("open the null device for OpenVMM", cause))
    };
    let stdin = null(OpenOptions::new().read(true))?;
    let stdout = null(OpenOptions::new().write(true))?;
    let (log, stderr) = match log {
        Some((path, file)) => (Some(path), OwnedHandle::from(file)),
        None => (None, null(OpenOptions::new().write(true))?),
    };
    let process = launch(&application, &directory, &job, [&stdin, &stdout, &stderr])
        .map_err(Error::host_file(HostFile::Vmm, Act::Start, &application))?;
    Ok(Vmm { job, process, log })
}

impl Vmm {
    /// Connects to the RPC socket once `OpenVMM` has bound it, watching for its exit meanwhile.
    ///
    /// # Errors
    /// Returns [`Error::VmmFailed`] if it exits first, cannot be watched, or `patience` runs out.
    pub(super) fn connect(&self, rpc: &Address, patience: Duration) -> Result<TcpStream, Error> {
        let deadline = Instant::now() + patience;
        loop {
            let refused = match afunix::connect_to(rpc) {
                Ok(socket) => return Ok(TcpStream::from(socket)),
                Err(cause) => cause,
            };
            let left = deadline.saturating_duration_since(Instant::now());
            match self.exited_within(left.min(PULSE)) {
                Ok(None) if left.is_zero() => {
                    return Err(self.failed(format!(
                        "had not bound its RPC socket {}s after it started; the last attempt to \
                         connect to it failed with: {refused}",
                        patience.as_secs()
                    )));
                }
                Ok(None) => {}
                Ok(Some(code)) => {
                    return Err(self.failed(format!("{} before it was ready", status(code))));
                }
                Err(cause) => {
                    return Err(
                        self.failed(format!("could not be watched while it started: {cause}"))
                    );
                }
            }
        }
    }

    /// The exit code, if the process has exited.
    pub(super) fn exited(&self) -> io::Result<Option<u32>> {
        self.exited_within(Duration::ZERO)
    }

    fn exited_within(&self, patience: Duration) -> io::Result<Option<u32>> {
        let millis = u32::try_from(patience.as_millis()).unwrap_or(INFINITE - 1);
        // SAFETY: a live process handle.
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), millis) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // SAFETY: a live, exited process and a writable slot.
                if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &raw mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Some(code))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// Ends the job, then gives the process `DYING_GRACE` to go.
    pub(super) fn kill(&self) -> io::Result<()> {
        // SAFETY: a live job handle; terminating our own job ends only OpenVMM.
        if unsafe { TerminateJobObject(self.job.as_raw_handle(), ERROR_PROCESS_ABORTED) } == 0 {
            return Err(io::Error::last_os_error());
        }
        match self.exited_within(DYING_GRACE)? {
            Some(_) => Ok(()),
            None => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "it was still running {}s after its job was ended",
                    DYING_GRACE.as_secs()
                ),
            )),
        }
    }

    /// `why`, unless the process exits within `DYING_GRACE`: then its exit `during` says more.
    pub(super) fn failed_unless_exited(&self, during: &str, why: String) -> Error {
        match self.exited_within(DYING_GRACE) {
            Ok(Some(code)) => self.failed(format!("{} {during}", status(code))),
            _ => self.failed(why),
        }
    }

    pub(super) fn log(&self) -> Option<&Path> {
        self.log.as_deref()
    }

    /// `OpenVMM`'s own failure, with the end of its log.
    pub(super) fn failed(&self, why: String) -> Error {
        Error::VmmFailed {
            why,
            log: self.log.clone(),
            tail: self.log.as_deref().map_or_else(Vec::new, tail),
        }
    }
}

impl Drop for Vmm {
    /// Reaps the process before anything removes what it held; closing the job would end it anyway.
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

/// An NTSTATUS crash code reads best in hex, an ordinary exit code in decimal.
pub(super) fn status(code: u32) -> String {
    if code >= 0xC000_0000 {
        format!("crashed with status {code:#010X}")
    } else {
        format!("exited with code {code}")
    }
}

fn tail(log: &Path) -> Vec<String> {
    let Ok(mut file) = site!(Silent::VmmLogTail, File::open(log)) else {
        return Vec::new();
    };
    let mut end = Vec::new();
    let read = file
        .metadata()
        .and_then(|meta| file.seek(SeekFrom::Start(meta.len().saturating_sub(TAIL_BYTES))))
        .and_then(|_| file.read_to_end(&mut end));
    if read.is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&end);
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(TAIL_LINES)..]
        .iter()
        .map(|line| (*line).to_owned())
        .collect()
}

fn kill_on_close_job() -> io::Result<OwnedHandle> {
    // SAFETY: null attributes make the job non-inheritable; a null name makes it anonymous.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle, owned from here on.
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: a live job and a readable limit structure of exactly the size given.
    let set = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            u32::try_from(size_of_val(&limits)).expect("a small struct"),
        )
    };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

/// Creates the process in `job`, inheriting exactly `stdio` and a three-variable environment.
fn launch(
    application: &Path,
    directory: &Path,
    job: &OwnedHandle,
    stdio: [&OwnedHandle; 3],
) -> io::Result<OwnedHandle> {
    let handles: [HANDLE; 3] = stdio.map(AsRawHandle::as_raw_handle);
    let jobs: [HANDLE; 1] = [job.as_raw_handle()];
    // Inheritable until the caller drops them, so a concurrent spawn may inherit them too;
    // neither readiness nor lifetime rests on them.
    for handle in handles {
        // SAFETY: a live handle; only its inheritance flag changes.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let application = win::wide(application.as_os_str());
    let directory = win::wide(directory.as_os_str());
    let mut command =
        win::wide(format!("openvmm.exe --rpc path={RPC_SOCKET},transport=ttrpc").as_ref());
    let environment = environment();

    let mut size = 0;
    // SAFETY: a size probe with a null list, which writes only `size`.
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &raw mut size) };
    // u64 storage aligns the opaque list.
    let mut list = vec![0u64; size.div_ceil(size_of::<u64>())];
    let attributes: LPPROC_THREAD_ATTRIBUTE_LIST = list.as_mut_ptr().cast();
    // SAFETY: a writable buffer of at least the probed size, for two attributes.
    if unsafe { InitializeProcThreadAttributeList(attributes, 2, 0, &raw mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut info = STARTUPINFOEXW::default();
    info.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>()).expect("a small struct");
    info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    [
        info.StartupInfo.hStdInput,
        info.StartupInfo.hStdOutput,
        info.StartupInfo.hStdError,
    ] = handles;
    info.lpAttributeList = attributes;
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: an initialised list naming `handles` and `jobs`, which outlive the call; every
    // string is NUL-terminated and live, and `command` is writable as CreateProcessW requires.
    let created = unsafe {
        let created = update(attributes, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &handles)
            && update(attributes, PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)
            && CreateProcessW(
                application.as_ptr(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                environment.as_ptr().cast(),
                directory.as_ptr(),
                (&raw const info).cast(),
                &raw mut process,
            ) != 0;
        let cause = io::Error::last_os_error();
        DeleteProcThreadAttributeList(attributes);
        created.then_some(()).ok_or(cause)
    };
    created?;
    // SAFETY: fresh handles from CreateProcessW, each owned once from here on.
    let (process, thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(process.hProcess),
            OwnedHandle::from_raw_handle(process.hThread),
        )
    };
    drop(thread);
    Ok(process)
}

/// Sets `attribute` to the array `handles`.
///
/// # Safety
/// `list` is initialised, and `handles` outlives every use of it.
unsafe fn update(list: LPPROC_THREAD_ATTRIBUTE_LIST, attribute: u32, handles: &[HANDLE]) -> bool {
    // SAFETY: the caller's; the size is exactly that of `handles`.
    unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            attribute as usize,
            handles.as_ptr().cast(),
            size_of_val(handles),
            std::ptr::null_mut(),
            std::ptr::null(),
        ) != 0
    }
}

/// `SystemRoot` for Winsock; `TEMP` and `TMP` for `UnixStream::pair`
/// (`support/unix_socket/src/windows.rs`).
fn environment() -> Vec<u16> {
    let mut block: Vec<u16> = ["SystemRoot", "TEMP", "TMP"]
        .into_iter()
        .filter_map(|name| {
            site!(Silent::VmmEnvironment, std::env::var_os(name)).map(|value| {
                name.encode_utf16()
                    .chain(Some(u16::from(b'=')))
                    .chain(value.encode_wide())
                    .chain(Some(0))
                    .collect::<Vec<u16>>()
            })
        })
        .flatten()
        .collect();
    // An empty block is still two NULs.
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}
