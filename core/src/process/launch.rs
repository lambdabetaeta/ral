//! Owned process-launch value.
//!
//! The runtime builds one `Launch` and hands it to the process subsystem
//! exactly once.  Unix lowers it back to `std::process::Command`; Windows owns
//! the raw `CreateProcessW` boundary so handle admission and Job Object
//! membership are creation-time facts.

use std::ffi::OsStr;
#[cfg(windows)]
use std::ffi::OsString;
use std::path::PathBuf;

#[cfg(windows)]
use windows_sys::Win32::Security::{PSID, SID_AND_ATTRIBUTES};

#[cfg(windows)]
pub(crate) use windows::RawChild;

#[cfg(not(windows))]
pub struct Launch {
    cmd: std::process::Command,
}

#[cfg(windows)]
pub struct Launch {
    program: OsString,
    args: Vec<OsString>,
    env: std::collections::BTreeMap<EnvKey, EnvEdit>,
    cwd: Option<PathBuf>,
    stdin: StdioSpec,
    stdout: StdioSpec,
    stderr: StdioSpec,
    creation_flags: u32,
    admitted_handles: Vec<std::os::windows::io::RawHandle>,
    security_capabilities: Option<SecurityCapabilitiesAttr>,
}

/// `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` payload for a confined
/// spawn: an AppContainer SID plus its capability-SID array. Kept as raw
/// `windows-sys` FFI values rather than the AppContainer profile / capability
/// types that build them (`sandbox::windows::appcontainer`) — this module
/// owns the spawn boundary and its attribute-list plumbing, not AppContainer
/// policy, so it only borrows the raw SID values the caller keeps alive.
#[cfg(windows)]
struct SecurityCapabilitiesAttr {
    app_container_sid: PSID,
    capabilities: Vec<SID_AND_ATTRIBUTES>,
}

#[cfg(windows)]
#[derive(Clone, Eq)]
struct EnvKey {
    folded: String,
    original: OsString,
}

#[cfg(windows)]
impl PartialEq for EnvKey {
    fn eq(&self, other: &Self) -> bool {
        self.folded == other.folded
    }
}

#[cfg(windows)]
impl PartialOrd for EnvKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(windows)]
impl Ord for EnvKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.folded.cmp(&other.folded)
    }
}

#[cfg(windows)]
enum EnvEdit {
    Set(OsString),
    Remove,
}

pub enum StdioSpec {
    Inherit,
    Null,
    Piped,
    #[cfg(not(windows))]
    Stdio(std::process::Stdio),
    #[cfg(windows)]
    Handle(std::os::windows::io::OwnedHandle),
}

impl StdioSpec {
    pub fn inherit() -> Self {
        Self::Inherit
    }

    pub fn null() -> Self {
        Self::Null
    }

    pub fn piped() -> Self {
        Self::Piped
    }

    #[cfg(not(windows))]
    fn into_stdio(self) -> std::process::Stdio {
        match self {
            Self::Inherit => std::process::Stdio::inherit(),
            Self::Null => std::process::Stdio::null(),
            Self::Piped => std::process::Stdio::piped(),
            Self::Stdio(stdio) => stdio,
        }
    }

    #[cfg(not(windows))]
    pub fn from_stdio(stdio: std::process::Stdio) -> Self {
        Self::Stdio(stdio)
    }

    #[cfg(windows)]
    pub fn from_owned_handle(handle: std::os::windows::io::OwnedHandle) -> Self {
        Self::Handle(handle)
    }

    pub fn from_pipe_reader(reader: os_pipe::PipeReader) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::io::OwnedHandle;
            Self::from_owned_handle(OwnedHandle::from(reader))
        }
        #[cfg(not(windows))]
        {
            Self::from_stdio(std::process::Stdio::from(reader))
        }
    }

    pub fn from_pipe_writer(writer: os_pipe::PipeWriter) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::io::OwnedHandle;
            Self::from_owned_handle(OwnedHandle::from(writer))
        }
        #[cfg(not(windows))]
        {
            Self::from_stdio(std::process::Stdio::from(writer))
        }
    }

    pub fn from_file(file: std::fs::File) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::io::OwnedHandle;
            Self::from_owned_handle(OwnedHandle::from(file))
        }
        #[cfg(not(windows))]
        {
            Self::from_stdio(std::process::Stdio::from(file))
        }
    }
}

#[cfg(not(windows))]
impl Launch {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        #[allow(
            clippy::disallowed_methods,
            reason = "[io-door:surface:process-launch] Builds the external exec image at ral's owned launch boundary; command::run emits the user-facing exec card with the resolved argv and status."
        )]
        let cmd = std::process::Command::new(program);
        Self { cmd }
    }

    /// Adopt an existing `Command`.
    ///
    /// Only the program, arguments, environment, and working directory are
    /// the launch's contract: stdio and redirections must be set on the
    /// returned `Launch`, never on the incoming `Command`. `std` exposes no
    /// stdio getters, so the Windows raw-`CreateProcessW` path cannot copy
    /// them across; treating them as uncarried on every platform keeps that a
    /// documented contract rather than a Windows-only surprise.
    pub fn from_command(cmd: std::process::Command) -> Self {
        Self { cmd }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.cmd.args(args);
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.env(key, val);
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.env_remove(key);
        self
    }

    pub fn current_dir(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        self.cmd.current_dir(dir.into());
        self
    }

    pub fn stdin(&mut self, stdio: StdioSpec) -> &mut Self {
        self.cmd.stdin(stdio.into_stdio());
        self
    }

    pub fn stdout(&mut self, stdio: StdioSpec) -> &mut Self {
        self.cmd.stdout(stdio.into_stdio());
        self
    }

    pub fn stderr(&mut self, stdio: StdioSpec) -> &mut Self {
        self.cmd.stderr(stdio.into_stdio());
        self
    }

    #[cfg(unix)]
    pub fn apply_unix_resource_limits(&mut self) {
        crate::sandbox::apply_resource_limits(&mut self.cmd);
    }

    #[cfg(unix)]
    pub fn dup_stdout_to_stderr(&mut self) {
        use std::os::unix::process::CommandExt;
        unsafe {
            self.cmd.pre_exec(|| {
                if libc::dup2(libc::STDOUT_FILENO, libc::STDERR_FILENO) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    #[cfg(unix)]
    pub fn clear_cloexec_on_spawn(&mut self, fd: std::os::fd::RawFd) {
        use std::os::unix::process::CommandExt;
        unsafe {
            self.cmd.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    /// Lower this launch to a `std::process::Command` and spawn it with the
    /// requested process-group placement, returning the child handle and its
    /// leader pgid.
    ///
    /// # Errors
    /// Returns `Err` if the spawn fails — the `fork`/`exec` itself or the
    /// pre-exec `setpgid`/`setsid` installed by [`spawn_with_pgid`](crate::process::spawn_with_pgid).
    pub fn spawn(
        &mut self,
        pgid: crate::process::PgidPolicy,
    ) -> std::io::Result<(crate::process::ChildHandle, Option<crate::process::Pgid>)> {
        let (child, pgid) = crate::process::spawn_with_pgid(&mut self.cmd, pgid)?;
        Ok((crate::process::ChildHandle::from_std(child), pgid))
    }
}

#[cfg(windows)]
impl Launch {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_os_string(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
            stdin: StdioSpec::Inherit,
            stdout: StdioSpec::Inherit,
            stderr: StdioSpec::Inherit,
            creation_flags: 0,
            admitted_handles: Vec::new(),
            security_capabilities: None,
        }
    }

    /// Adopt an existing `Command`.
    ///
    /// Only the program, arguments, environment, and working directory are
    /// carried: `std` exposes no stdio getters, so any stdio or redirections
    /// set on the incoming `Command` are dropped and must be re-set on the
    /// returned `Launch`. This is a documented contract, matching the
    /// non-Windows arm.
    pub fn from_command(cmd: std::process::Command) -> Self {
        let mut launch = Self::new(cmd.get_program());
        launch.args(cmd.get_args());
        for (key, value) in cmd.get_envs() {
            match value {
                Some(value) => {
                    launch.env(key, value);
                }
                None => {
                    launch.env_remove(key);
                }
            }
        }
        if let Some(cwd) = cmd.get_current_dir() {
            launch.current_dir(cwd);
        }
        launch
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_os_string()));
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.env.insert(
            env_key(key.as_ref()),
            EnvEdit::Set(val.as_ref().to_os_string()),
        );
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.env.insert(env_key(key.as_ref()), EnvEdit::Remove);
        self
    }

    pub fn current_dir(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn stdin(&mut self, stdio: StdioSpec) -> &mut Self {
        self.stdin = stdio;
        self
    }

    pub fn stdout(&mut self, stdio: StdioSpec) -> &mut Self {
        self.stdout = stdio;
        self
    }

    pub fn stderr(&mut self, stdio: StdioSpec) -> &mut Self {
        self.stderr = stdio;
        self
    }

    pub fn creation_flags(&mut self, flags: u32) -> &mut Self {
        self.creation_flags |= flags;
        self
    }

    pub fn admit_handle(&mut self, handle: std::os::windows::io::RawHandle) {
        if !self.admitted_handles.contains(&handle) {
            self.admitted_handles.push(handle);
        }
    }

    /// Attach `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` to the spawn: an
    /// AppContainer SID plus its capability-SID array (built by
    /// `sandbox::windows::appcontainer` — an `AppContainerProfile`'s SID and
    /// a `CapabilitySids`' entries). `app_container_sid` and every SID inside
    /// `capabilities` must stay valid until [`Self::spawn`] returns: this
    /// only borrows their raw values into the attribute list at spawn time,
    /// it does not take ownership.  The per-command sandbox launcher
    /// (`sandbox::windows::session::confine`) threads the projection's
    /// session-owned profile SID and capability array through here.
    pub fn security_capabilities(
        &mut self,
        app_container_sid: PSID,
        capabilities: &[SID_AND_ATTRIBUTES],
    ) -> &mut Self {
        self.security_capabilities = Some(SecurityCapabilitiesAttr {
            app_container_sid,
            capabilities: capabilities.to_vec(),
        });
        self
    }

    /// Lower this launch through the raw `CreateProcessW` boundary and spawn
    /// it with the requested process-group placement, returning the child
    /// handle and its leader pgid.
    ///
    /// # Errors
    /// Returns `Err` if the program is a `.bat`/`.cmd` file (refused), if any
    /// argument, path, or environment entry contains a NUL, if handle
    /// admission or the `CreateProcessW` call fails, or if placing the child
    /// in its pipeline Job Object fails.
    pub fn spawn(
        &mut self,
        pgid: crate::process::PgidPolicy,
    ) -> std::io::Result<(crate::process::ChildHandle, Option<crate::process::Pgid>)> {
        windows::spawn(self, pgid)
    }
}

#[cfg(windows)]
fn env_key(key: &OsStr) -> EnvKey {
    EnvKey {
        folded: key.to_string_lossy().to_ascii_uppercase(),
        original: key.to_os_string(),
    }
}

#[cfg(windows)]
mod windows_args {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;

    pub(super) fn make_command_line(
        program: &OsStr,
        args: &[std::ffi::OsString],
    ) -> io::Result<Vec<u16>> {
        ensure_no_nuls(program)?;
        let mut cmd = Vec::new();
        cmd.push(b'"' as u16);
        cmd.extend(program.encode_wide());
        cmd.push(b'"' as u16);
        for arg in args {
            cmd.push(b' ' as u16);
            append_arg(&mut cmd, arg)?;
        }
        cmd.push(0);
        Ok(cmd)
    }

    fn append_arg(cmd: &mut Vec<u16>, arg: &OsStr) -> io::Result<()> {
        ensure_no_nuls(arg)?;
        let units: Vec<u16> = arg.encode_wide().collect();
        let quote = units.is_empty()
            || units
                .iter()
                .any(|c| *c == b' ' as u16 || *c == b'\t' as u16);
        if quote {
            cmd.push(b'"' as u16);
        }
        let mut backslashes = 0usize;
        for unit in units {
            if unit == b'\\' as u16 {
                backslashes += 1;
            } else {
                if unit == b'"' as u16 {
                    cmd.extend(std::iter::repeat(b'\\' as u16).take(backslashes + 1));
                }
                backslashes = 0;
            }
            cmd.push(unit);
        }
        if quote {
            cmd.extend(std::iter::repeat(b'\\' as u16).take(backslashes));
            cmd.push(b'"' as u16);
        }
        Ok(())
    }

    pub(super) fn ensure_no_nuls(s: &OsStr) -> io::Result<()> {
        if s.encode_wide().any(|c| c == 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows process argument contains a NUL byte",
            ));
        }
        Ok(())
    }

    // ── Unicode round-trip (item W3.8) ──────────────────────────────────
    //
    // `make_command_line`/`append_arg` own the UTF-16 conversion at the
    // spawn boundary; these tests exercise that seam directly (no spawn
    // needed) with non-ASCII *and* non-BMP (astral-plane, surrogate-pair)
    // text, so a regression that mangles wide-char conversion — not just
    // quoting — is caught. `cfg(windows)`: `OsStrExt::encode_wide` doesn't
    // exist off Windows, so this only builds and runs on Windows CI.
    #[cfg(test)]
    mod tests {
        use super::*;

        /// Decode a `make_command_line` buffer back to a `String`,
        /// dropping the trailing NUL `make_command_line` always appends.
        fn decode(wide: &[u16]) -> String {
            let (last, body) = wide.split_last().expect("non-empty command line");
            assert_eq!(*last, 0, "make_command_line must NUL-terminate");
            String::from_utf16(body).expect("well-formed UTF-16")
        }

        /// A program path and a lone plain argument (no space/quote/
        /// backslash, so `append_arg` takes its no-quoting branch and the
        /// wide buffer is exactly the UTF-16 re-encoding of the input) —
        /// isolates the encode/decode round-trip from the quoting rules.
        #[test]
        fn plain_arg_round_trips_non_ascii_and_non_bmp() {
            let program = std::ffi::OsString::from(r"C:\Users\café\bin\日本語.exe");
            let arg = std::ffi::OsString::from("🎉𝔘𝔫𝔦𝔠𝔬𝔡𝔢-café-日本語");
            let cmd = make_command_line(program.as_os_str(), std::slice::from_ref(&arg)).unwrap();
            let decoded = decode(&cmd);
            assert_eq!(
                decoded,
                format!(
                    "\"{}\" {}",
                    program.to_str().unwrap(),
                    arg.to_str().unwrap()
                )
            );
        }

        /// The same text, but the argument also contains a space — forces
        /// `append_arg`'s quoting branch, so this proves the astral-plane
        /// characters survive being wrapped in quotes too, not just the
        /// unquoted path.
        #[test]
        fn quoted_arg_round_trips_non_ascii_and_non_bmp() {
            let program = std::ffi::OsString::from(r"C:\Program Files\ral.exe");
            let arg = std::ffi::OsString::from("🎉 café 日本語 with spaces");
            let cmd = make_command_line(program.as_os_str(), std::slice::from_ref(&arg)).unwrap();
            let decoded = decode(&cmd);
            assert_eq!(
                decoded,
                format!(
                    "\"{}\" \"{}\"",
                    program.to_str().unwrap(),
                    arg.to_str().unwrap()
                )
            );
        }

        /// A surrogate-pair character (astral plane) sitting directly
        /// against the arg boundary and against an escaped embedded quote
        /// — the two spots most likely to split a surrogate pair if the
        /// backslash-doubling logic walked `u16` units carelessly. Built
        /// with `format!`/interpolation rather than hand-escaped string
        /// literals, so the expected value is unambiguous to read.
        #[test]
        fn non_bmp_survives_adjacent_to_escaped_quote() {
            let q = '"';
            let program = std::ffi::OsString::from(r"C:\ral.exe");
            let arg = std::ffi::OsString::from(format!("🎉{q}🎊 quoted 🎊{q}🎉"));
            let cmd = make_command_line(program.as_os_str(), std::slice::from_ref(&arg)).unwrap();
            let decoded = decode(&cmd);
            // Quoted (embedded spaces) with each embedded `"` escaped by
            // one preceding backslash.
            let expected_payload = format!("🎉\\{q}🎊 quoted 🎊\\{q}🎉");
            assert_eq!(
                decoded,
                format!("\"{}\" \"{expected_payload}\"", program.to_str().unwrap())
            );
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::{EnvEdit, Launch, StdioSpec, windows_args};
    use crate::process::{ChildHandle, Pgid, PgidPolicy};
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::ptr::{null, null_mut};
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::SECURITY_CAPABILITIES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
        InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, ResumeThread,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    static LAUNCH_LOCK: Mutex<()> = Mutex::new(());

    pub(super) fn spawn(
        launch: &mut Launch,
        policy: PgidPolicy,
    ) -> io::Result<(ChildHandle, Option<Pgid>)> {
        reject_batch(&launch.program)?;
        let prepared = crate::process::signal::prepare_group(policy)?;
        match spawn_inner(launch, policy, &prepared) {
            Ok(child) => {
                let pgid = crate::process::signal::register_prepared_group(prepared, &child);
                Ok((child, pgid))
            }
            Err(err) => {
                crate::process::signal::close_prepared_group(prepared);
                Err(err)
            }
        }
    }

    fn spawn_inner(
        launch: &mut Launch,
        policy: PgidPolicy,
        prepared: &crate::process::signal::PreparedGroup,
    ) -> io::Result<ChildHandle> {
        let stdin = ChildStdio::lower(
            std::mem::replace(&mut launch.stdin, StdioSpec::Null),
            Stream::Stdin,
        )?;
        let mut stdout = ChildStdio::lower(
            std::mem::replace(&mut launch.stdout, StdioSpec::Null),
            Stream::Stdout,
        )?;
        let mut stderr = ChildStdio::lower(
            std::mem::replace(&mut launch.stderr, StdioSpec::Null),
            Stream::Stderr,
        )?;

        let mut inherited = vec![stdin.child, stdout.child, stderr.child];
        inherited.extend(launch.admitted_handles.iter().map(|h| *h as HANDLE));
        inherited.sort();
        inherited.dedup();

        let mut cmdline = windows_args::make_command_line(&launch.program, &launch.args)?;
        let application = wide_null(&launch.program)?;
        let cwd = launch
            .cwd
            .as_ref()
            .map(|p| wide_null(p.as_os_str()))
            .transpose()?;
        let mut env = environment_block(&launch.env)?;

        // The handle list is always present; the security-capabilities
        // attribute is present only when a caller has staged an
        // AppContainer spawn via `Launch::security_capabilities`.
        let attr_count: u32 = if launch.security_capabilities.is_some() {
            2
        } else {
            1
        };
        let mut attrs = AttributeList::new(attr_count)?;
        attrs.update_handle_list(&inherited)?;

        // Build the SECURITY_CAPABILITIES value now (rather than inside
        // `AttributeList`) so its address is a plain local that outlives the
        // `CreateProcessW` call below without any self-referential storage:
        // the `Capabilities` pointer inside it borrows
        // `launch.security_capabilities`'s own `Vec`, which `launch` keeps
        // alive for this whole function.
        let security_caps_value: Option<SECURITY_CAPABILITIES> = launch
            .security_capabilities
            .as_mut()
            .map(|caps| SECURITY_CAPABILITIES {
                AppContainerSid: caps.app_container_sid,
                Capabilities: caps.capabilities.as_mut_ptr(),
                CapabilityCount: caps.capabilities.len() as u32,
                Reserved: 0,
            });
        if let Some(value) = security_caps_value.as_ref() {
            attrs.update_security_capabilities(value)?;
        }

        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = stdin.child;
        startup.StartupInfo.hStdOutput = stdout.child;
        startup.StartupInfo.hStdError = stderr.child;
        startup.lpAttributeList = attrs.as_mut_ptr();

        let mut flags =
            launch.creation_flags | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
        if matches!(
            policy,
            PgidPolicy::NewLeader | PgidPolicy::NewSession | PgidPolicy::Join(_)
        ) {
            flags |= CREATE_NEW_PROCESS_GROUP;
        }
        if crate::process::signal::prepared_job(prepared).is_some() {
            flags |= CREATE_SUSPENDED;
        }

        let _guard = LAUNCH_LOCK.lock().unwrap();
        let inheritable = InheritableHandles::set(&inherited)?;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            CreateProcessW(
                application.as_ptr(),
                cmdline.as_mut_ptr(),
                null(),
                null(),
                1,
                flags,
                env.as_mut_ptr() as *const _,
                cwd.as_ref().map(|w| w.as_ptr()).unwrap_or(null()),
                &raw const startup.StartupInfo,
                &raw mut pi,
            )
        };
        drop(inheritable);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        let process = OwnedHandleGuard(pi.hProcess);
        let thread = OwnedHandleGuard(pi.hThread);
        if let Some(job) = crate::process::signal::prepared_job(prepared) {
            let assigned = unsafe { AssignProcessToJobObject(job, process.0) };
            if assigned == 0 {
                unsafe {
                    windows_sys::Win32::System::Threading::TerminateProcess(process.0, 1);
                }
                return Err(io::Error::other(
                    "could not place pipeline child in its Job Object before start; is ral already running in a restrictive Windows Job Object?",
                ));
            }
            if unsafe { ResumeThread(thread.0) } == u32::MAX {
                unsafe {
                    windows_sys::Win32::System::Threading::TerminateProcess(process.0, 1);
                }
                return Err(io::Error::last_os_error());
            }
        }

        let process = process.into_owned();
        drop(thread);
        Ok(ChildHandle::from_windows_raw(RawChild::new(
            process,
            pi.dwProcessId,
            stdout.parent_stdout.take(),
            stderr.parent_stderr.take(),
        )))
    }

    /// Refuse to launch a `.bat`/`.cmd` image outright — the correct
    /// posture for CVE-2024-24576 (Rust std's own `.bat`/`.cmd` argument
    /// quoting could be broken out of by a crafted argument, because
    /// batch-file argument quoting has no single safe encoding: `cmd.exe`
    /// re-interprets the command line through its own escaping rules,
    /// layered on top of `CreateProcessW`'s). Rather than synthesize a
    /// `cmd /c` wrapper — which is exactly the unsafe quoting problem,
    /// one layer removed — this launcher declines the image entirely
    /// until a from-scratch, tested port of that quoting exists (see the
    /// ADR at `docs/ral-wiki/decisions/260702_windows-spawn-boundary.md`,
    /// §"Treat command-line quoting as security").
    #[allow(
        clippy::disallowed_methods,
        reason = "FFI glue: the image name arrives as the `&OsStr` bound for CreateProcessW; the crate::path helpers are `&str`-shaped, and lossy-decoding a path to classify its extension would be the real hazard"
    )]
    fn reject_batch(program: &OsStr) -> io::Result<()> {
        let Some(ext) = std::path::Path::new(program).extension() else {
            return Ok(());
        };
        let ext = ext.to_string_lossy();
        if ext.eq_ignore_ascii_case("bat") || ext.eq_ignore_ascii_case("cmd") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "refusing to launch '{}': .bat/.cmd images are not supported on the \
                     raw Windows launch path. ral will not synthesize a `cmd /c` wrapper \
                     to run it, because batch-file argument quoting has no safe general \
                     encoding (the CVE-2024-24576 class of bug) — a crafted argument could \
                     inject additional commands through cmd.exe's own escaping rules. Invoke \
                     the batch file through cmd.exe yourself if you accept that risk, or run \
                     the underlying program directly.",
                    std::path::Path::new(program).display()
                ),
            ));
        }
        Ok(())
    }

    #[derive(Clone, Copy)]
    enum Stream {
        Stdin,
        Stdout,
        Stderr,
    }

    struct ChildStdio {
        child: HANDLE,
        parent_stdout: Option<std::fs::File>,
        parent_stderr: Option<std::fs::File>,
        _owned_child: Option<OwnedHandle>,
    }

    impl ChildStdio {
        fn lower(spec: StdioSpec, stream: Stream) -> io::Result<Self> {
            match spec {
                StdioSpec::Inherit => {
                    let handle = match stream {
                        Stream::Stdin => unsafe {
                            windows_sys::Win32::System::Console::GetStdHandle(
                                windows_sys::Win32::System::Console::STD_INPUT_HANDLE,
                            )
                        },
                        Stream::Stdout => unsafe {
                            windows_sys::Win32::System::Console::GetStdHandle(
                                windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE,
                            )
                        },
                        Stream::Stderr => unsafe {
                            windows_sys::Win32::System::Console::GetStdHandle(
                                windows_sys::Win32::System::Console::STD_ERROR_HANDLE,
                            )
                        },
                    };
                    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                        return Self::null(stream);
                    }
                    Ok(Self {
                        child: handle,
                        parent_stdout: None,
                        parent_stderr: None,
                        _owned_child: None,
                    })
                }
                StdioSpec::Null => Self::null(stream),
                StdioSpec::Piped => Self::piped(stream),
                StdioSpec::Handle(handle) => {
                    let raw = handle.as_raw_handle() as HANDLE;
                    Ok(Self {
                        child: raw,
                        parent_stdout: None,
                        parent_stderr: None,
                        _owned_child: Some(handle),
                    })
                }
            }
        }

        fn null(stream: Stream) -> io::Result<Self> {
            let access = match stream {
                Stream::Stdin => FILE_GENERIC_READ,
                Stream::Stdout | Stream::Stderr => FILE_GENERIC_WRITE,
            };
            let handle = open_nul(access)?;
            let raw = handle.as_raw_handle() as HANDLE;
            Ok(Self {
                child: raw,
                parent_stdout: None,
                parent_stderr: None,
                _owned_child: Some(handle),
            })
        }

        fn piped(stream: Stream) -> io::Result<Self> {
            let (reader, writer) = os_pipe::pipe()?;
            match stream {
                Stream::Stdin => {
                    let raw = reader.as_raw_handle() as HANDLE;
                    Ok(Self {
                        child: raw,
                        parent_stdout: None,
                        parent_stderr: None,
                        _owned_child: Some(pipe_reader_to_owned(reader)),
                    })
                }
                Stream::Stdout => {
                    let raw = writer.as_raw_handle() as HANDLE;
                    let parent = pipe_reader_to_file(reader);
                    Ok(Self {
                        child: raw,
                        parent_stdout: Some(parent),
                        parent_stderr: None,
                        _owned_child: Some(pipe_writer_to_owned(writer)),
                    })
                }
                Stream::Stderr => {
                    let raw = writer.as_raw_handle() as HANDLE;
                    let parent = pipe_reader_to_file(reader);
                    Ok(Self {
                        child: raw,
                        parent_stdout: None,
                        parent_stderr: Some(parent),
                        _owned_child: Some(pipe_writer_to_owned(writer)),
                    })
                }
            }
        }
    }

    fn open_nul(access: u32) -> io::Result<OwnedHandle> {
        let name = wide_null(OsStr::new("NUL"))?;
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `CreateFileW` returned a fresh owning handle.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) })
    }

    fn pipe_reader_to_owned(reader: os_pipe::PipeReader) -> OwnedHandle {
        OwnedHandle::from(reader)
    }

    fn pipe_writer_to_owned(writer: os_pipe::PipeWriter) -> OwnedHandle {
        OwnedHandle::from(writer)
    }

    fn pipe_reader_to_file(reader: os_pipe::PipeReader) -> std::fs::File {
        std::fs::File::from(OwnedHandle::from(reader))
    }

    struct AttributeList {
        buf: Vec<u8>,
    }

    impl AttributeList {
        /// Allocate an attribute list sized for exactly `attribute_count`
        /// `UpdateProcThreadAttribute` entries. The count must match the
        /// number of `update_*` calls the caller goes on to make — Win32
        /// sizes the underlying buffer from it up front.
        fn new(attribute_count: u32) -> io::Result<Self> {
            let mut bytes = 0usize;
            unsafe {
                InitializeProcThreadAttributeList(null_mut(), attribute_count, 0, &raw mut bytes);
            }
            let mut buf = vec![0; bytes];
            let ok = unsafe {
                InitializeProcThreadAttributeList(
                    buf.as_mut_ptr() as *mut _,
                    attribute_count,
                    0,
                    &raw mut bytes,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { buf })
        }

        fn as_mut_ptr(&mut self) -> *mut std::ffi::c_void {
            self.buf.as_mut_ptr() as *mut _
        }

        fn update_handle_list(&mut self, handles: &[HANDLE]) -> io::Result<()> {
            let ok = unsafe {
                UpdateProcThreadAttribute(
                    self.as_mut_ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr() as *const _,
                    std::mem::size_of_val(handles),
                    null_mut(),
                    null(),
                )
            };
            if ok == 0 {
                return Err(io::Error::other(
                    "could not launch pipeline helper with an explicit handle list",
                ));
            }
            Ok(())
        }

        fn update_security_capabilities(
            &mut self,
            value: &SECURITY_CAPABILITIES,
        ) -> io::Result<()> {
            let ok = unsafe {
                UpdateProcThreadAttribute(
                    self.as_mut_ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                    value as *const SECURITY_CAPABILITIES as *const _,
                    std::mem::size_of::<SECURITY_CAPABILITIES>(),
                    null_mut(),
                    null(),
                )
            };
            if ok == 0 {
                return Err(io::Error::other(
                    "could not launch sandboxed child with AppContainer security capabilities",
                ));
            }
            Ok(())
        }
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            unsafe {
                DeleteProcThreadAttributeList(self.as_mut_ptr());
            }
        }
    }

    struct InheritableHandles(Vec<HANDLE>);

    impl InheritableHandles {
        fn set(handles: &[HANDLE]) -> io::Result<Self> {
            for &handle in handles {
                let ok = unsafe {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT)
                };
                if ok == 0 {
                    for &done in handles.iter().take_while(|h| **h != handle) {
                        unsafe {
                            SetHandleInformation(done, HANDLE_FLAG_INHERIT, 0);
                        }
                    }
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(Self(handles.to_vec()))
        }
    }

    impl Drop for InheritableHandles {
        fn drop(&mut self) {
            for &handle in &self.0 {
                unsafe {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    }

    struct OwnedHandleGuard(HANDLE);

    impl OwnedHandleGuard {
        fn into_owned(self) -> OwnedHandle {
            let handle = self.0;
            std::mem::forget(self);
            // SAFETY: `CreateProcessW` returned a fresh owning process handle.
            unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) }
        }
    }

    impl Drop for OwnedHandleGuard {
        fn drop(&mut self) {
            unsafe {
                if !self.0.is_null() {
                    CloseHandle(self.0);
                }
            }
        }
    }

    fn wide_null(s: &OsStr) -> io::Result<Vec<u16>> {
        windows_args::ensure_no_nuls(s)?;
        Ok(s.encode_wide().chain([0]).collect())
    }

    fn environment_block(
        edits: &std::collections::BTreeMap<super::EnvKey, EnvEdit>,
    ) -> io::Result<Vec<u16>> {
        let mut env = std::collections::BTreeMap::<super::EnvKey, OsString>::new();
        for (k, v) in std::env::vars_os() {
            env.insert(super::env_key(&k), v);
        }
        for (k, edit) in edits {
            match edit {
                EnvEdit::Set(v) => {
                    env.insert(k.clone(), v.clone());
                }
                EnvEdit::Remove => {
                    env.remove(k);
                }
            }
        }
        let mut block = Vec::new();
        for (k, v) in env {
            windows_args::ensure_no_nuls(&k.original)?;
            windows_args::ensure_no_nuls(&v)?;
            block.extend(k.original.encode_wide());
            block.push(b'=' as u16);
            block.extend(v.encode_wide());
            block.push(0);
        }
        block.push(0);
        Ok(block)
    }

    // ── Raw CreateProcessW child ───────────────────────────────────────────
    //
    // The owning handle to a child spawned through this module's raw
    // `CreateProcessW` boundary, plus the wait/reap methods `ChildHandle`
    // dispatches to on Windows.

    pub(crate) struct RawChild {
        process: OwnedHandle,
        pid: u32,
        stdout: Option<std::fs::File>,
        stderr: Option<std::fs::File>,
    }

    impl RawChild {
        pub(crate) fn new(
            process: OwnedHandle,
            pid: u32,
            stdout: Option<std::fs::File>,
            stderr: Option<std::fs::File>,
        ) -> Self {
            Self {
                process,
                pid,
                stdout,
                stderr,
            }
        }

        pub(crate) fn id(&self) -> u32 {
            self.pid
        }

        pub(crate) fn raw_process_handle(&self) -> HANDLE {
            self.process.as_raw_handle() as HANDLE
        }

        pub(crate) fn kill(&mut self) -> io::Result<()> {
            let ok = unsafe {
                windows_sys::Win32::System::Threading::TerminateProcess(
                    self.raw_process_handle(),
                    1,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub(crate) fn take_stdout(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
            self.stdout
                .take()
                .map(|stdout| Box::new(stdout) as Box<dyn std::io::Read + Send>)
        }

        pub(crate) fn take_stderr(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
            self.stderr
                .take()
                .map(|stderr| Box::new(stderr) as Box<dyn std::io::Read + Send>)
        }

        /// Block until the process exits and read its exit status — the
        /// shared body of [`Self::wait_handling_stop`] and [`Self::reap`],
        /// which differ only in how they map that status onward (a
        /// [`crate::process::WaitOutcome`] vs. the raw
        /// [`std::process::ExitStatus`] `ChildHandle::reap` needs
        /// cross-platform).
        fn wait_and_exit_status(&self) -> io::Result<std::process::ExitStatus> {
            let r = unsafe { WaitForSingleObject(self.raw_process_handle(), INFINITE) };
            if r != WAIT_OBJECT_0 {
                return Err(io::Error::last_os_error());
            }
            self.exit_status()
        }

        pub(crate) fn wait_handling_stop(&mut self) -> io::Result<crate::process::WaitOutcome> {
            self.wait_and_exit_status()
                .map(crate::process::WaitOutcome::from_exit_status)
        }

        pub(crate) fn try_wait_handling_stop(
            &mut self,
        ) -> io::Result<Option<crate::process::WaitOutcome>> {
            match unsafe { WaitForSingleObject(self.raw_process_handle(), 0) } {
                WAIT_OBJECT_0 => self
                    .exit_status()
                    .map(crate::process::WaitOutcome::from_exit_status)
                    .map(Some),
                WAIT_TIMEOUT => Ok(None),
                _ => Err(io::Error::last_os_error()),
            }
        }

        pub(crate) fn reap(&mut self) -> io::Result<std::process::ExitStatus> {
            self.wait_and_exit_status()
        }

        fn exit_status(&self) -> io::Result<std::process::ExitStatus> {
            use std::os::windows::process::ExitStatusExt;
            let mut code = 0;
            let ok = unsafe { GetExitCodeProcess(self.raw_process_handle(), &raw mut code) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(std::process::ExitStatus::from_raw(code))
        }
    }

    // ── Unicode round-trip (item W3.8) ──────────────────────────────────
    //
    // `wide_null` owns the program-path/cwd UTF-16 conversion and
    // `environment_block` owns the env-block conversion; both are tested
    // directly with non-ASCII and non-BMP (surrogate-pair) text. Only
    // builds and runs on Windows CI (`OsStrExt::encode_wide` is
    // Windows-only).
    #[cfg(test)]
    mod tests {
        use super::*;

        fn decode_wide_null(v: &[u16]) -> String {
            let (last, body) = v.split_last().expect("non-empty");
            assert_eq!(*last, 0, "wide_null must NUL-terminate");
            String::from_utf16(body).expect("well-formed UTF-16")
        }

        #[test]
        fn wide_null_round_trips_non_ascii_and_non_bmp() {
            let s = "🎉𝔘𝔫𝔦𝔠𝔬𝔡𝔢-café-日本語";
            let encoded = wide_null(OsStr::new(s)).unwrap();
            assert_eq!(decode_wide_null(&encoded), s);
        }

        #[test]
        fn wide_null_round_trips_a_non_ascii_program_path() {
            let path = r"C:\Users\café\日本語プロジェクト\ral.exe";
            let encoded = wide_null(OsStr::new(path)).unwrap();
            assert_eq!(decode_wide_null(&encoded), path);
        }

        /// `environment_block` layers `edits` over the live process
        /// environment; the round-trip claim under test is only about the
        /// UTF-16 conversion of one injected non-ASCII/non-BMP entry, so
        /// the assertion searches the decoded block for that entry's
        /// `KEY=value\0` run rather than comparing the whole block (which
        /// also carries whatever the test process inherited).
        #[test]
        fn environment_block_round_trips_a_non_ascii_non_bmp_value() {
            let key = super::super::env_key(OsStr::new("RAL_UNICODE_TEST_VAR"));
            let value = OsString::from("🎉𝔘𝔫𝔦𝔠𝔬𝔡𝔢-café-日本語");
            let edits = std::collections::BTreeMap::from([(key, EnvEdit::Set(value.clone()))]);
            let block = environment_block(&edits).unwrap();
            // The block is a flat run of NUL-terminated `KEY=value` pairs
            // plus one extra trailing NUL marking the end of the whole
            // block; strip that outer terminator, then decode and split
            // on the per-entry NULs to search.
            let (_, body) = block.split_last().expect("non-empty");
            let text = String::from_utf16(body).expect("well-formed UTF-16");
            let expected = format!("RAL_UNICODE_TEST_VAR={}", value.to_str().unwrap());
            assert!(
                text.split('\u{0}').any(|entry| entry == expected),
                "expected entry {expected:?} not found in environment block"
            );
        }
    }
}
