//! The owned process-launch value, built once and handed over once.
//!
//! Unix lowers it back to `std::process::Command`; Windows drives
//! `CreateProcessW` itself, because handle admission and Job Object
//! membership must be creation-time facts.

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
    jail: Option<crate::process::jail::JailCgroup>,
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

/// `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` payload, borrowing raw SID
/// values from the types in `sandbox::windows::appcontainer` that own them.
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
        Self { cmd, jail: None }
    }

    /// Adopt an existing `Command`.  Set stdio and redirections on the
    /// returned `Launch`, never on the incoming one: `std` exposes no stdio
    /// getters, so the Windows arm cannot carry them across.
    pub fn from_command(cmd: std::process::Command) -> Self {
        Self { cmd, jail: None }
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

    /// Prepare this exec's transient cgroup and register the pre-exec that
    /// puts the child in it at the plan's unprivileged uid/gid.  Called from
    /// `build_command` in `runtime/command/process.rs` when the shell carries
    /// a [`GuestJail`](crate::process::jail::GuestJail).
    ///
    /// # Errors
    /// The kernel's error for any mkdir/write/open the setup performs.
    #[cfg(target_os = "linux")]
    pub(crate) fn apply_guest_jail(
        &mut self,
        plan: &crate::process::jail::JailPlan,
    ) -> std::io::Result<()> {
        let (cgroup, procs) = crate::process::jail::linux::prepare(plan)?;
        crate::process::jail::linux::install_pre_exec(&mut self.cmd, plan, procs);
        self.jail = Some(cgroup);
        Ok(())
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

    /// Lower to a `std::process::Command` and spawn it with the requested
    /// process-group placement: child, leader pgid, and whatever jail cgroup
    /// `apply_guest_jail` staged.
    ///
    /// # Errors
    /// The `fork`/`exec` itself, or the pre-exec `setpgid`/`setsid` that
    /// `process::signal::spawn_with_pgid` installs.
    pub fn spawn(
        &mut self,
        pgid: crate::process::PgidPolicy,
    ) -> std::io::Result<(
        crate::process::ChildHandle,
        Option<crate::process::Pgid>,
        Option<crate::process::jail::JailCgroup>,
    )> {
        let (child, pgid) = crate::process::spawn_with_pgid(&mut self.cmd, pgid)?;
        Ok((
            crate::process::ChildHandle::from_std(child),
            pgid,
            self.jail.take(),
        ))
    }

    /// Spawn *detached* by double fork: the survivor is this process's
    /// grandchild, reparented onto init in a session of its own, and only its
    /// pid comes back.  Nothing here can then name it — the whole difference
    /// from [`Self::spawn`].
    ///
    /// So point all three standard streams at something that outlives this
    /// process; an inherited one dies with us, taking the survivor's writes
    /// with it.  And under a guest jail the survivor keeps its `exec-N`
    /// cgroup and its limits for good, since nobody left can tell when that
    /// cgroup is empty.
    ///
    /// # Errors
    /// A failed fork or `execve`, or a pid handshake that comes up short.
    #[cfg(unix)]
    pub(crate) fn spawn_detached(&mut self) -> std::io::Result<u32> {
        crate::process::signal::spawn_detached(&mut self.cmd)
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

    /// Adopt an existing `Command` — program, args, environment, cwd.  `std`
    /// exposes no stdio getters, so stdio set on it is dropped and must be
    /// re-set here, exactly as on the non-Windows arm.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the non-Windows arm moves the `Command` into the launch it returns; both arms take it by value so the shared callers see one cross-platform signature"
    )]
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

    /// Stage `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` for the spawn.
    /// Only the raw SID values are borrowed, so they must stay valid until
    /// [`Self::spawn`] returns; `sandbox::windows::session::confine` passes
    /// SIDs the session owns for the whole process lifetime.
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

    /// Lower through `CreateProcessW` and spawn with the requested
    /// process-group placement.  The jail cgroup is always `None` — Linux
    /// guests only — and stays in the tuple to keep callers uniform.
    ///
    /// # Errors
    /// A `.bat`/`.cmd` program (refused), a NUL in any argument, path, or
    /// environment entry, a failed handle admission or `CreateProcessW`, or a
    /// failure to place the child in its pipeline Job Object.
    pub fn spawn(
        &mut self,
        pgid: crate::process::PgidPolicy,
    ) -> std::io::Result<(
        crate::process::ChildHandle,
        Option<crate::process::Pgid>,
        Option<crate::process::jail::JailCgroup>,
    )> {
        windows::spawn(self, pgid).map(|(child, pgid)| (child, pgid, None))
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
        cmd.push(u16::from(b'"'));
        cmd.extend(program.encode_wide());
        cmd.push(u16::from(b'"'));
        for arg in args {
            cmd.push(u16::from(b' '));
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
                .any(|c| *c == u16::from(b' ') || *c == u16::from(b'\t'));
        if quote {
            cmd.push(u16::from(b'"'));
        }
        let mut backslashes = 0usize;
        for unit in units {
            if unit == u16::from(b'\\') {
                backslashes += 1;
            } else {
                if unit == u16::from(b'"') {
                    cmd.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes + 1));
                }
                backslashes = 0;
            }
            cmd.push(unit);
        }
        if quote {
            cmd.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
            cmd.push(u16::from(b'"'));
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

    // ── Unicode round-trip ──────────────────────────────────────────────
    //
    // Non-BMP text through the argv UTF-16 conversion, so a wide-char
    // regression is caught apart from a quoting one.  Windows CI only.
    #[cfg(test)]
    mod tests {
        use super::*;

        /// Decode a `make_command_line` buffer, dropping its trailing NUL.
        fn decode(wide: &[u16]) -> String {
            let (last, body) = wide.split_last().expect("non-empty command line");
            assert_eq!(*last, 0, "make_command_line must NUL-terminate");
            String::from_utf16(body).expect("well-formed UTF-16")
        }

        /// A plain argument takes `append_arg`'s no-quoting branch, isolating
        /// the encode/decode round-trip from the quoting rules.
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

        /// The same text through `append_arg`'s quoting branch.
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

        /// A surrogate pair against the arg boundary and against an escaped
        /// embedded quote — the two places careless `u16`-unit walking in the
        /// backslash-doubling would split one.
        #[test]
        fn non_bmp_survives_adjacent_to_escaped_quote() {
            let q = '"';
            let program = std::ffi::OsString::from(r"C:\ral.exe");
            let arg = std::ffi::OsString::from(format!("🎉{q}🎊 quoted 🎊{q}🎉"));
            let cmd = make_command_line(program.as_os_str(), std::slice::from_ref(&arg)).unwrap();
            let decoded = decode(&cmd);
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

    /// Marking a handle inheritable is process-global state, so only one
    /// spawn at a time may hold that window open across `CreateProcessW`.
    static LAUNCH_LOCK: Mutex<()> = Mutex::new(());

    /// `STARTUPINFOEXW`'s own byte count, as the `u32` its `cb` field wants.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a fixed Win32 layout; size_of is a compile-time constant of ~112, nowhere near u32::MAX"
    )]
    const STARTUPINFOEXW_CB: u32 = std::mem::size_of::<STARTUPINFOEXW>() as u32;

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

        let attr_count: u32 = if launch.security_capabilities.is_some() {
            2
        } else {
            1
        };
        let mut attrs = AttributeList::new(attr_count)?;
        attrs.update_handle_list(&inherited)?;

        // A plain local, so its address outlives `CreateProcessW` below with
        // no self-reference: `Capabilities` points into `launch`'s own `Vec`.
        let security_caps_value: Option<SECURITY_CAPABILITIES> = launch
            .security_capabilities
            .as_mut()
            .map(|caps| SECURITY_CAPABILITIES {
                AppContainerSid: caps.app_container_sid,
                Capabilities: caps.capabilities.as_mut_ptr(),
                CapabilityCount: u32::try_from(caps.capabilities.len())
                    .expect("a projection declares a handful of capability SIDs, not four billion"),
                Reserved: 0,
            });
        if let Some(value) = security_caps_value.as_ref() {
            attrs.update_security_capabilities(value)?;
        }

        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = STARTUPINFOEXW_CB;
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
                cwd.as_ref().map_or(null(), std::vec::Vec::as_ptr),
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

    /// Refuse `.bat`/`.cmd` images outright: `cmd.exe` re-interprets the
    /// command line through its own escaping rules on top of
    /// `CreateProcessW`'s, so batch argument quoting has no safe general
    /// encoding (the CVE-2024-24576 class).  A `cmd /c` wrapper only moves it.
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
        /// Allocate for exactly `attribute_count` entries — Win32 sizes the
        /// buffer up front, so it must match the `update_*` calls that follow.
        fn new(attribute_count: u32) -> io::Result<Self> {
            let mut bytes = 0usize;
            unsafe {
                InitializeProcThreadAttributeList(null_mut(), attribute_count, 0, &raw mut bytes);
            }
            let mut buf = vec![0; bytes];
            let ok = unsafe {
                InitializeProcThreadAttributeList(
                    buf.as_mut_ptr().cast(),
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
            self.buf.as_mut_ptr().cast()
        }

        fn update_handle_list(&mut self, handles: &[HANDLE]) -> io::Result<()> {
            let ok = unsafe {
                UpdateProcThreadAttribute(
                    self.as_mut_ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr().cast(),
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
                    std::ptr::from_ref::<SECURITY_CAPABILITIES>(value).cast(),
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
            block.push(u16::from(b'='));
            block.extend(v.encode_wide());
            block.push(0);
        }
        block.push(0);
        Ok(block)
    }

    // ── Raw CreateProcessW child ───────────────────────────────────────────
    //
    // The owning handle to a child born above, plus the wait/reap methods
    // `ChildHandle` dispatches to.

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

        /// Block until exit and read the status: [`Self::wait_handling_stop`]
        /// and [`Self::reap`] differ only in how they map it onward.
        fn wait_and_exit_status(&self) -> io::Result<std::process::ExitStatus> {
            let r = unsafe { WaitForSingleObject(self.raw_process_handle(), INFINITE) };
            if r != WAIT_OBJECT_0 {
                return Err(io::Error::last_os_error());
            }
            self.exit_status()
        }

        #[allow(
            clippy::needless_pass_by_ref_mut,
            reason = "mirrors `ChildHandle::wait_handling_stop`, whose `std::process::Child` arm dispatches beside this one and does need `&mut`; a wait is exclusive by contract even where Windows reaches the exit status through a shared handle"
        )]
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

        #[allow(
            clippy::needless_pass_by_ref_mut,
            reason = "mirrors `ChildHandle::reap`, whose `std::process::Child` arm calls the raw `Child::wait`; reaping is an exclusive act even where Windows performs it through a shared handle"
        )]
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

    // ── Unicode round-trip ──────────────────────────────────────────────
    //
    // The other two UTF-16 conversions: `wide_null` for the program path and
    // cwd, `environment_block` for the env block.
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

        /// `environment_block` layers `edits` over the live environment, so
        /// the assertion hunts the block for the one injected entry.
        #[test]
        fn environment_block_round_trips_a_non_ascii_non_bmp_value() {
            let key = super::super::env_key(OsStr::new("RAL_UNICODE_TEST_VAR"));
            let value = OsString::from("🎉𝔘𝔫𝔦𝔠𝔬𝔡𝔢-café-日本語");
            let edits = std::collections::BTreeMap::from([(key, EnvEdit::Set(value.clone()))]);
            let block = environment_block(&edits).unwrap();
            // Entries are NUL-terminated, plus one extra NUL closing the whole
            // block: strip that, then split on the per-entry NULs.
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
