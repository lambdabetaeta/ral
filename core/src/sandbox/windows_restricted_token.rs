//! Windows restricted-token + Low-IL backend for confined eval.
//!
//! The model is the Chrome-renderer pattern:
//!
//! 1. Duplicate the parent's primary token, drop every privilege via
//!    `CreateRestrictedToken(DISABLE_MAX_PRIVILEGE, ...)`, and add a single
//!    restricting SID (`NT AUTHORITY\RESTRICTED`).  The kernel runs a
//!    secondary access check against the restricting set, so anything whose
//!    DACL doesn't explicitly grant `RESTRICTED` (typical SSH keys, browser
//!    profiles, user-private files) becomes unreadable to the child.
//! 2. Lower the token's integrity level to Low (`S-1-16-4096`) via
//!    `SetTokenInformation(TokenIntegrityLevel, ...)`.  Windows denies
//!    writes from a Low-IL caller to anything labelled Medium-or-above —
//!    which is everything by default.  That kills `rm -rf`-style attacks.
//! 3. `CreateProcessAsUserW` the child under that token with the parent's
//!    `STARTUPINFOEX` carrying a process-creation mitigation policy
//!    attribute (BlockNonMicroSigned + BottomUpASLR + DEP + HeapTerminate).
//! 4. The runner assigns the child to a Job Object with
//!    `KILL_ON_JOB_CLOSE` + UI restrictions before resuming.
//!
//! The setup is **O(1)** in policy size — no DACLs are written to the
//! user's tree, no registry profile is created, and there is nothing to
//! revoke on drop except the token handle and the per-spawn scratch dir.
//!
//! The child also gets a per-spawn writable scratch directory under
//! `std::env::temp_dir()` whose mandatory label is stamped to Low so the
//! Low-IL child can actually write to it.  `lpCurrentDirectory` points at
//! this directory; everything else in the user's tree is read-only via the
//! integrity-level write barrier.
//!
//! **Policy mapping note:** `policy.fs.{read,write,deny}_paths` and
//! `policy.net` are *advisory* under this backend — they are surfaced in
//! `dump_profile_for_windows` for audit and `policy.exec` is still gated
//! in-process before the spawn, but the kernel-side enforcement comes
//! entirely from the restricted token + Low IL + Job Object combination.
//! Per-path read/write/deny is a no-op; net is a no-op until a future
//! WFP integration.

use crate::types::{Break, Error, SandboxProjection};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
use windows_sys::Win32::Security::{
    ACL, ACL_REVISION, AddMandatoryAce, CreateRestrictedToken, CreateWellKnownSid,
    DISABLE_MAX_PRIVILEGE, DuplicateTokenEx, InitializeAcl, LABEL_SECURITY_INFORMATION, PSID,
    SID_AND_ATTRIBUTES, SecurityImpersonation, SetTokenInformation, TOKEN_ADJUST_DEFAULT,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel, TokenPrimary, WinLowLabelSid,
    WinRestrictedCodeSid,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CREATE_SUSPENDED, CreateProcessAsUserW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess,
    InitializeProcThreadAttributeList, OpenProcessToken, PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
    PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

/// `SE_GROUP_*` flag enabling a SID in a token's restricting-SID array.
/// Not re-exported under a stable name in `windows-sys` 0.61.
const SE_GROUP_ENABLED: u32 = 0x4;

/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` =
/// `ProcThreadAttributeValue(ProcThreadAttributeHandleList=2, FALSE, TRUE,
/// FALSE)` = `0x0002_0002`.  Not re-exported by `windows-sys` 0.61.
/// Bounds the handles the child inherits to exactly the listed set, so
/// `bInheritHandles = TRUE` passes only the handles named in this list.
const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x0002_0002;

/// Process-creation mitigation flags we pass via
/// `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY`.  `windows-sys` 0.61 does not
/// re-export the named `PROCESS_CREATION_MITIGATION_POLICY_*` constants,
/// so the bit definitions live here.  Reference values from
/// `processthreadsapi.h` in the Windows SDK.
const PROCESS_CREATION_MITIGATION_POLICY_DEP_ENABLE: u64 = 0x0000_0000_0000_0001;
const PROCESS_CREATION_MITIGATION_POLICY_BOTTOM_UP_ASLR_ALWAYS_ON: u64 = 0x0000_0000_0001_0000;
const PROCESS_CREATION_MITIGATION_POLICY_HEAP_TERMINATE_ALWAYS_ON: u64 = 0x0000_0000_0000_0010;
/// `PROCESS_CREATION_MITIGATION_POLICY_BLOCK_NON_MICROSOFT_BINARIES_ALWAYS_ON`
/// = 1 << 44.  Constant is not in `windows-sys` 0.61.
const PROCESS_CREATION_MITIGATION_POLICY_BLOCK_NON_MICROSOFT_BINARIES_ALWAYS_ON: u64 =
    0x0000_1000_0000_0000;

/// `CREATE_UNICODE_ENVIRONMENT` — required when `lpEnvironment` is wide.
const CREATE_UNICODE_ENVIRONMENT: u32 = 0x400;

/// `SYSTEM_MANDATORY_LABEL_NO_WRITE_UP` (not exposed in `windows-sys`
/// 0.61 as a named constant in `Win32::Security`).
const SYSTEM_MANDATORY_LABEL_NO_WRITE_UP: u32 = 0x1;

/// `SE_GROUP_INTEGRITY` — flag set on the integrity-label SID inside a
/// `TOKEN_MANDATORY_LABEL` structure.  Not exposed under that name in
/// `windows-sys` 0.61.
const SE_GROUP_INTEGRITY: u32 = 0x20;

/// Convert a `Path` to a NUL-terminated wide buffer.
fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain([0]).collect()
}

fn stage_err(stage: &str, e: impl std::fmt::Display) -> Break {
    Break::Error(Error::new(format!("sandbox eval: {stage}: {e}"), 1))
}

/// Probe whether the restricted-token backend is usable on this host.
///
/// Mirrors `confined_availability` on Unix: cheap, in-process, doesn't
/// touch the user's filesystem.  Opens our own primary token with the
/// minimal access set we'll need at spawn time; success means the
/// kernel is willing to hand us a duplicable token, which is effectively
/// always true on Windows 8+.
pub(super) fn backend_ready() -> bool {
    unsafe {
        let mut h: HANDLE = std::ptr::null_mut();
        let ok = OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY,
            &mut h as *mut HANDLE,
        );
        if ok == 0 {
            return false;
        }
        CloseHandle(h);
    }
    true
}

/// Allocate a well-known SID into a fresh byte buffer.  The returned
/// `Vec<u8>` is the backing storage; the SID lives at its start.
unsafe fn well_known_sid(kind: i32) -> Result<Vec<u8>, Break> {
    // SECURITY_MAX_SID_SIZE is 68 on Windows; use 80 for headroom.
    let mut buf: Vec<u8> = vec![0u8; 80];
    let mut size: u32 = buf.len() as u32;
    let ok = unsafe {
        CreateWellKnownSid(
            kind,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as PSID,
            &mut size as *mut u32,
        )
    };
    if ok == 0 {
        return Err(stage_err(
            "CreateWellKnownSid",
            format!("error {}", unsafe { GetLastError() }),
        ));
    }
    buf.truncate(size as usize);
    Ok(buf)
}

/// RAII wrapper that closes a `HANDLE` on drop.  Used to keep partially-
/// built tokens alive across `?` short-circuits in `new_for`.
struct CloseOnDrop(HANDLE);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Per-confined-eval restricted Low-IL primary token plus its scratch dir.
///
/// `Drop` closes the token handle and best-effort recursively removes the
/// scratch dir.  Nothing on disk outside the scratch dir is touched at any
/// point in this type's lifetime.
pub(super) struct RestrictedTokenSpec {
    token: HANDLE,
    scratch: PathBuf,
}

impl RestrictedTokenSpec {
    /// Build a restricted, Low-integrity primary token and a labelled
    /// scratch directory for one confined eval.
    pub(super) fn new_for(_policy: &SandboxProjection) -> Result<Self, Break> {
        // ── 1. open the parent's primary token ──────────────────────────
        let parent_token: HANDLE = unsafe {
            let mut h: HANDLE = std::ptr::null_mut();
            let ok = OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE
                    | TOKEN_ASSIGN_PRIMARY
                    | TOKEN_QUERY
                    | TOKEN_ADJUST_DEFAULT
                    | TOKEN_ADJUST_PRIVILEGES
                    | TOKEN_ADJUST_SESSIONID,
                &mut h as *mut HANDLE,
            );
            if ok == 0 {
                return Err(stage_err(
                    "OpenProcessToken",
                    format!("error {}", GetLastError()),
                ));
            }
            h
        };
        let _parent_guard = CloseOnDrop(parent_token);

        // ── 2. build the RESTRICTED SID and feed it to CreateRestrictedToken ──
        let restricted_sid_buf = unsafe { well_known_sid(WinRestrictedCodeSid)? };
        let restricting = [SID_AND_ATTRIBUTES {
            Sid: restricted_sid_buf.as_ptr() as PSID,
            Attributes: SE_GROUP_ENABLED,
        }];
        let restricted_token: HANDLE = unsafe {
            let mut h: HANDLE = std::ptr::null_mut();
            let ok = CreateRestrictedToken(
                parent_token,
                DISABLE_MAX_PRIVILEGE,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                restricting.len() as u32,
                restricting.as_ptr(),
                &mut h as *mut HANDLE,
            );
            if ok == 0 {
                return Err(stage_err(
                    "CreateRestrictedToken",
                    format!("error {}", GetLastError()),
                ));
            }
            h
        };
        let _restricted_guard = CloseOnDrop(restricted_token);

        // ── 3. duplicate to a *primary* token suitable for CreateProcessAsUserW ──
        let primary_token: HANDLE = unsafe {
            let mut h: HANDLE = std::ptr::null_mut();
            let ok = DuplicateTokenEx(
                restricted_token,
                // 0 == use the source token's access mask
                0,
                std::ptr::null(),
                SecurityImpersonation,
                TokenPrimary,
                &mut h as *mut HANDLE,
            );
            if ok == 0 {
                return Err(stage_err(
                    "DuplicateTokenEx",
                    format!("error {}", GetLastError()),
                ));
            }
            h
        };
        // primary_token is now owned by `self` on success.  Until the
        // final `Ok(...)`, an early return must close it; the guard
        // handles that, and we `forget` it just before returning.
        let primary_guard = CloseOnDrop(primary_token);

        // ── 4. lower integrity level to Low ─────────────────────────────
        let low_sid_buf = unsafe { well_known_sid(WinLowLabelSid)? };
        let label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: low_sid_buf.as_ptr() as PSID,
                Attributes: SE_GROUP_INTEGRITY,
            },
        };
        let ok = unsafe {
            SetTokenInformation(
                primary_token,
                TokenIntegrityLevel,
                &label as *const TOKEN_MANDATORY_LABEL as *const core::ffi::c_void,
                std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + low_sid_buf.len() as u32,
            )
        };
        if ok == 0 {
            return Err(stage_err(
                "SetTokenInformation(TokenIntegrityLevel)",
                format!("error {}", unsafe { GetLastError() }),
            ));
        }

        // ── 5. create the per-spawn scratch directory ───────────────────
        let pid = std::process::id();
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let scratch = std::env::temp_dir().join(format!("ral-sandbox-{pid}-{ns:032x}"));
        std::fs::create_dir_all(&scratch).map_err(|e| stage_err("create scratch dir", e))?;

        // ── 6. stamp the scratch dir with a Low mandatory label ─────────
        // Build a fresh SACL holding one SYSTEM_MANDATORY_LABEL_ACE
        // pointing at the Low IL SID, then hand it to
        // SetNamedSecurityInfoW under LABEL_SECURITY_INFORMATION.
        let sid_len = low_sid_buf.len();
        // ACL header (8 bytes) + SYSTEM_MANDATORY_LABEL_ACE
        //   = ACE_HEADER (4) + Mask (4) + SID inline (sid_len, less the
        //     leading u32 placeholder `SidStart` already counted... but
        //     AddMandatoryAce takes care of sizing, so over-allocate).
        let acl_len: u32 = (8 + 4 + 4 + sid_len) as u32;
        let mut acl_buf: Vec<u8> = vec![0u8; acl_len as usize];
        let acl_ptr = acl_buf.as_mut_ptr() as *mut ACL;
        let ok = unsafe { InitializeAcl(acl_ptr, acl_len, ACL_REVISION) };
        if ok == 0 {
            return Err(stage_err(
                "InitializeAcl(scratch SACL)",
                format!("error {}", unsafe { GetLastError() }),
            ));
        }
        let ok = unsafe {
            AddMandatoryAce(
                acl_ptr,
                ACL_REVISION,
                0,
                SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
                low_sid_buf.as_ptr() as PSID,
            )
        };
        if ok == 0 {
            return Err(stage_err(
                "AddMandatoryAce",
                format!("error {}", unsafe { GetLastError() }),
            ));
        }

        let scratch_w = wide_path(&scratch);
        let err = unsafe {
            SetNamedSecurityInfoW(
                scratch_w.as_ptr(),
                SE_FILE_OBJECT,
                LABEL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
                acl_ptr as *const ACL,
            )
        };
        if err != 0 {
            return Err(stage_err(
                "SetNamedSecurityInfoW(scratch label)",
                format!("error {err}"),
            ));
        }

        // primary_token survives — detach the guard.
        std::mem::forget(primary_guard);

        Ok(Self {
            token: primary_token,
            scratch,
        })
    }

    /// The restricted Low-IL primary token.  Borrow only; the spec owns
    /// the handle and closes it on drop.
    pub(super) fn token(&self) -> HANDLE {
        self.token
    }

    /// The per-spawn Low-IL writable scratch directory.  `lpCurrentDirectory`
    /// in `CreateProcessAsUserW` points here, so the child has *some*
    /// writable cwd despite the Low-IL write barrier.
    pub(super) fn scratch_dir(&self) -> &Path {
        &self.scratch
    }
}

impl Drop for RestrictedTokenSpec {
    fn drop(&mut self) {
        unsafe {
            if !self.token.is_null() && self.token != INVALID_HANDLE_VALUE {
                CloseHandle(self.token);
            }
        }
        // Best-effort scratch cleanup; the moniker is unique per spawn,
        // so leaks here are inert.
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

/// Spawned confined child.  Owns the process / thread handles; the Job
/// Object that reaps the process is owned by the caller (`runner.rs`).
pub(super) struct Spawned {
    pub process: HANDLE,
    pub thread: HANDLE,
}

impl Drop for Spawned {
    fn drop(&mut self) {
        unsafe {
            if !self.process.is_null() && self.process != INVALID_HANDLE_VALUE {
                // Belt-and-braces: if the child is still alive on unwind,
                // terminate it.  The Job Object handles the same case via
                // `KILL_ON_JOB_CLOSE`, but doing both removes the race
                // window where the process handle outlives the job.
                let mut code: u32 = 0;
                if GetExitCodeProcess(self.process, &mut code as *mut u32) != 0 && code == 259
                /* STILL_ACTIVE */
                {
                    TerminateProcess(self.process, 1);
                }
                CloseHandle(self.process);
            }
            if !self.thread.is_null() && self.thread != INVALID_HANDLE_VALUE {
                CloseHandle(self.thread);
            }
        }
    }
}

impl Spawned {
    /// Block until the child exits, returning its exit code.
    pub(super) fn wait(&self) -> Result<i32, Break> {
        const INFINITE: u32 = 0xFFFF_FFFF;
        const WAIT_FAILED: u32 = 0xFFFF_FFFF;
        let r = unsafe { WaitForSingleObject(self.process, INFINITE) };
        if r == WAIT_FAILED {
            // On WAIT_FAILED the diagnostic lives in GetLastError, not in
            // r (which is just the sentinel).
            return Err(stage_err(
                "WaitForSingleObject",
                format!("error {}", unsafe { GetLastError() }),
            ));
        }
        if r != 0 {
            return Err(stage_err("WaitForSingleObject", format!("wait result {r}")));
        }
        let mut code: u32 = 0;
        if unsafe { GetExitCodeProcess(self.process, &mut code as *mut u32) } == 0 {
            return Err(stage_err(
                "GetExitCodeProcess",
                format!("error {}", unsafe { GetLastError() }),
            ));
        }
        Ok(code as i32)
    }
}

/// Append `arg` to a `CreateProcessW`-style command line, quoting per the
/// `CommandLineToArgvW`/MSVCRT rules so the child's CRT recovers `arg`
/// verbatim.  A run of backslashes is doubled only when it precedes a
/// `"` — including the synthesised closing quote — so a quoted argument
/// ending in a backslash (a trailing path separator, a JSON blob) does
/// not let its final `\` escape the terminating quote.  `force` quotes
/// even an arg that contains no whitespace; otherwise quoting is added
/// only when `arg` is empty or contains a space, tab, or `"`.
fn append_quoted(cmdline: &mut String, arg: &str, force: bool) {
    let needs_quotes = force || arg.is_empty() || arg.contains([' ', '\t', '"']);
    if !needs_quotes {
        cmdline.push_str(arg);
        return;
    }
    cmdline.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                cmdline.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                backslashes = 0;
                cmdline.push('"');
            }
            _ => {
                cmdline.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                cmdline.push(ch);
            }
        }
    }
    cmdline.extend(std::iter::repeat_n('\\', backslashes * 2));
    cmdline.push('"');
}

/// Spawn `exec_path` under `spec`'s restricted token, with `argv_tail`
/// appended after `arg0`.  The child starts suspended; the caller (the
/// runner) assigns it to the Job Object and then resumes the main thread.
///
/// `inherit` is the complete inheritable set in the order
/// `[stdin, stdout, stderr, ipc]` — the std handles plus the IPC pipe,
/// the latter inheritable transiently inside
/// [`super::ipc::IpcEndpoint::lend_for_spawn`].  It is installed as a
/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` so `bInheritHandles = TRUE` passes
/// only these four handles to the child.
pub(super) fn spawn_confined(
    exec_path: &Path,
    arg0: &Path,
    argv_tail: &[String],
    spec: &RestrictedTokenSpec,
    inherit: [HANDLE; 4],
    env_block: Option<&[u16]>,
) -> Result<Spawned, Break> {
    let [inherit_stdin, inherit_stdout, inherit_stderr, _] = inherit;
    // Build the command line.  CreateProcessAsUserW expects a mutable
    // wide buffer; per docs, argv[0] should be quoted if it contains
    // spaces.
    let mut cmdline = String::new();
    append_quoted(&mut cmdline, &arg0.display().to_string(), true);
    for arg in argv_tail {
        cmdline.push(' ');
        append_quoted(&mut cmdline, arg, false);
    }
    let mut cmdline_w: Vec<u16> = OsStr::new(&cmdline).encode_wide().chain([0]).collect();

    let exec_w = wide_path(exec_path);
    let cwd_w = wide_path(spec.scratch_dir());

    // Build the attribute list.  Two attributes: MITIGATION_POLICY and a
    // HANDLE_LIST bounding inheritance to the std handles plus the IPC
    // pipe.
    let mut size: usize = 0;
    unsafe {
        // First call obtains the required buffer size; expected to fail
        // with ERROR_INSUFFICIENT_BUFFER, hence the unchecked return.
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &mut size as *mut usize);
    }
    let mut attr_buf: Vec<u8> = vec![0u8; size];
    let attr_list = attr_buf.as_mut_ptr() as *mut core::ffi::c_void;
    let ok = unsafe { InitializeProcThreadAttributeList(attr_list, 2, 0, &mut size as *mut usize) };
    if ok == 0 {
        return Err(stage_err(
            "InitializeProcThreadAttributeList",
            format!("error {}", unsafe { GetLastError() }),
        ));
    }

    // Mitigation policy mask is a u64; the kernel reads 8 bytes when
    // cbSize == 8.  `windows-sys` 0.61 declares
    // `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY` as a `u32`, so the
    // *attribute identifier* is u32 but the *attribute value* is u64.
    let mitigation_mask: u64 = PROCESS_CREATION_MITIGATION_POLICY_DEP_ENABLE
        | PROCESS_CREATION_MITIGATION_POLICY_BOTTOM_UP_ASLR_ALWAYS_ON
        | PROCESS_CREATION_MITIGATION_POLICY_HEAP_TERMINATE_ALWAYS_ON
        | PROCESS_CREATION_MITIGATION_POLICY_BLOCK_NON_MICROSOFT_BINARIES_ALWAYS_ON;
    let ok = unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY as usize,
            &mitigation_mask as *const u64 as *const core::ffi::c_void,
            std::mem::size_of::<u64>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        unsafe { DeleteProcThreadAttributeList(attr_list) };
        return Err(stage_err(
            "UpdateProcThreadAttribute(mitigation)",
            format!("error {err}"),
        ));
    }

    // Bound inheritance to exactly the std handles plus the IPC pipe.  The
    // array must outlive CreateProcessAsUserW, which reads it through the
    // attribute list.
    let ok = unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            inherit.as_ptr() as *const core::ffi::c_void,
            std::mem::size_of_val(&inherit),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        unsafe { DeleteProcThreadAttributeList(attr_list) };
        return Err(stage_err(
            "UpdateProcThreadAttribute(handle list)",
            format!("error {err}"),
        ));
    }

    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = inherit_stdin;
    si.StartupInfo.hStdOutput = inherit_stdout;
    si.StartupInfo.hStdError = inherit_stderr;
    si.lpAttributeList = attr_list;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let mut creation_flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_NEW_CONSOLE;
    if env_block.is_some() {
        creation_flags |= CREATE_UNICODE_ENVIRONMENT;
    }
    let env_ptr = env_block
        .map(|e| e.as_ptr() as *const core::ffi::c_void)
        .unwrap_or(std::ptr::null());

    let ok = unsafe {
        CreateProcessAsUserW(
            spec.token(),
            exec_w.as_ptr(),
            cmdline_w.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // bInheritHandles = TRUE
            creation_flags,
            env_ptr,
            cwd_w.as_ptr(),
            &si.StartupInfo as *const STARTUPINFOW,
            &mut pi as *mut PROCESS_INFORMATION,
        )
    };
    let last_err = unsafe { GetLastError() };
    unsafe { DeleteProcThreadAttributeList(attr_list) };
    if ok == 0 {
        return Err(stage_err(
            "CreateProcessAsUserW",
            format!("error {last_err}"),
        ));
    }

    Ok(Spawned {
        process: pi.hProcess,
        thread: pi.hThread,
    })
}

/// Resume the suspended main thread of a confined child.  Caller invokes
/// this *after* assigning the child to the Job Object.
pub(super) fn resume(spawned: &Spawned) -> Result<(), Break> {
    let r = unsafe { ResumeThread(spawned.thread) };
    if r == u32::MAX {
        return Err(stage_err(
            "ResumeThread",
            format!("error {}", unsafe { GetLastError() }),
        ));
    }
    Ok(())
}

/// Render a human-readable dump of the planned restricted-token + Low IL
/// + scratch-dir plan for `RAL_DUMP_SANDBOX_PROFILE`.  Mirrors the
/// Seatbelt SBPL / bwrap argv dumps but for the Win32 path.
pub fn dump_profile_for_windows(policy: &SandboxProjection) -> String {
    let mut out = String::new();
    out.push_str("Restricted-token plan:\n");
    out.push_str("  integrity level: Low (S-1-16-4096)\n");
    out.push_str("  restricting SIDs: [S-1-5-12 RESTRICTED]\n");
    out.push_str(
        "  scratch dir: per-spawn under std::env::temp_dir() as \
ral-sandbox-<pid>-<ns>; stamped Low via SACL\n",
    );
    out.push_str("  mitigation flags:\n");
    out.push_str("    DEP_ENABLE\n");
    out.push_str("    BOTTOM_UP_ASLR_ALWAYS_ON\n");
    out.push_str("    HEAP_TERMINATE_ALWAYS_ON\n");
    out.push_str("    BLOCK_NON_MICROSOFT_BINARIES_ALWAYS_ON\n");
    out.push_str("  ignored (advisory under this backend):\n");
    out.push_str(&format!(
        "    net: {} -- NOT enforced; WFP integration is a TODO\n",
        if policy.net { "requested" } else { "deny" }
    ));
    if let Some(fs) = policy.fs.as_policy() {
        out.push_str("    fs.read_prefixes (logged only, not enforced as allow-list):\n");
        for p in &fs.read_prefixes {
            out.push_str(&format!("      {}\n", p.as_str()));
        }
        out.push_str(
            "    fs.write_prefixes (logged only, not enforced; only scratch dir writable):\n",
        );
        for p in &fs.write_prefixes {
            out.push_str(&format!("      {}\n", p.as_str()));
        }
        out.push_str("    fs.deny_paths (logged only, not enforced):\n");
        for p in &fs.deny_paths {
            out.push_str(&format!("      {}\n", p.as_str()));
        }
    } else {
        out.push_str("    fs: unrestricted (Low IL still blocks writes to Medium+ objects)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::append_quoted;

    fn quoted(arg: &str) -> String {
        let mut s = String::new();
        append_quoted(&mut s, arg, false);
        s
    }

    #[test]
    fn trailing_backslash_does_not_escape_closing_quote() {
        // A path ending in `\` inside a quoted arg: the run of backslashes
        // before the synthesised closing quote must be doubled so the CRT
        // does not read the final `\"` as an escaped quote.
        assert_eq!(quoted("C:\\foo\\"), "\"C:\\foo\\\\\"");
    }

    #[test]
    fn backslashes_before_embedded_quote_are_doubled() {
        // `a\"b` → backslash run before the literal `"` doubled, then the
        // quote itself escaped.
        assert_eq!(quoted("a\\\"b"), "\"a\\\\\\\"b\"");
    }

    #[test]
    fn no_special_chars_is_left_bare() {
        assert_eq!(quoted("plain"), "plain");
    }

    #[test]
    fn interior_backslashes_are_not_doubled() {
        // Backslashes not adjacent to a quote pass through unchanged.
        assert_eq!(quoted("a\\b c"), "\"a\\b c\"");
    }

    #[test]
    fn empty_arg_becomes_empty_quotes() {
        assert_eq!(quoted(""), "\"\"");
    }
}
