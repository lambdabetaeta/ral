//! Hidden helper modes for process-staged pipelines.
//!
//! `--ral-pipeline-stage-helper` runs one ral stage in a fresh child
//! shell.  The parent packs the stage and the ambient shell snapshot
//! into a [`StageJob`]; the helper reconstructs the shell, optionally
//! reads one typed upstream value from `VALUE_IN_FD_ENV`, runs the
//! stage via `invoke`, and emits a structured [`ChildEvalResponse`] (with
//! the final value embedded for the parent to recover after the helper
//! exits).
//!
//! `--ral-pipeline-anchor` owns the pipeline pgid for the whole launch
//! so a fast-exiting first stage cannot strand later stages.

use crate::child_eval::{
    ChildEvalRequest, ChildEvalResponse, ChildKind, break_response, report_without_eval,
    run_child_eval, transfer_error,
};
use crate::evaluator::audit;
use crate::io::TerminalState;
use crate::runtime::command;
use crate::serial::{InternCtx, ScopeTable, SerialValue, build_arcs};
use crate::subprocess_codec::{read_frame, write_frame};
use crate::syntax::ast::RedirectMode;
use crate::types::{
    Break, CapturePolicy, EnvVars, Error, GrantStack, LocationCursor, Settled, Shell, Value,
};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

/// Hidden multicall sentinel for one pipeline-stage helper subprocess.
pub(crate) const HELPER_FLAG: &str = "--ral-pipeline-stage-helper";

#[cfg(unix)]
pub(crate) const JOB_FD_ENV: &str = "RAL_PIPELINE_STAGE_JOB_FD";
#[cfg(unix)]
pub(crate) const REPORT_FD_ENV: &str = "RAL_PIPELINE_STAGE_REPORT_FD";
#[cfg(unix)]
pub(crate) const VALUE_IN_FD_ENV: &str = "RAL_PIPELINE_STAGE_VALUE_IN_FD";
#[cfg(unix)]
pub(crate) const VALUE_OUT_FD_ENV: &str = "RAL_PIPELINE_STAGE_VALUE_OUT_FD";
#[cfg(unix)]
pub(crate) const ANCHOR_FD_ENV: &str = "RAL_PIPELINE_ANCHOR_FD";

#[cfg(windows)]
pub(crate) const JOB_HANDLE_ENV: &str = "RAL_PIPELINE_STAGE_JOB_HANDLE";
#[cfg(windows)]
pub(crate) const REPORT_HANDLE_ENV: &str = "RAL_PIPELINE_STAGE_REPORT_HANDLE";
#[cfg(windows)]
pub(crate) const VALUE_IN_HANDLE_ENV: &str = "RAL_PIPELINE_STAGE_VALUE_IN_HANDLE";
#[cfg(windows)]
pub(crate) const VALUE_OUT_HANDLE_ENV: &str = "RAL_PIPELINE_STAGE_VALUE_OUT_HANDLE";

/// Hidden multicall sentinel for the stable pipeline pgid anchor.
pub(crate) const ANCHOR_FLAG: &str = "--ral-pipeline-anchor";

/// A multicall sentinel that no live caller emits.  Recognising it here
/// turns a stale invocation — an old script, or a re-exec from a
/// mismatched binary — into a clear diagnostic and a non-zero exit
/// rather than an opaque clap usage error.
pub(crate) const RETIRED_EXEC_FLAG: &str = "--ral-pipeline-exec-helper";

/// The job frame the parent gates a stage helper on: a ral computation
/// wrapping a shared [`ChildEvalRequest`], or a terminal bundled-tool
/// invocation that never enters the evaluator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum StageJob {
    Ral(ChildEvalRequest),
    Uutils {
        tool: String,
        args: Vec<String>,
        redirects: Vec<WireRedirect>,
        ambient: UutilsSnapshot,
    },
}

/// The subset of [`Context`](crate::types::Context) a terminal uutils
/// helper needs.  Mirrors the runtime field names so a reader can
/// trace the IPC payload back to its source without a translation
/// table.
///
/// Deliberately excludes the ral evaluator payload: no `Comp`,
/// captured environment, registry, modules, or handlers cross this arm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UutilsSnapshot {
    pub env_overrides: EnvVars,
    pub dir: Option<PathBuf>,
    pub grants: GrantStack,
    pub location: LocationCursor,
    /// Carried separately from the mobile-shaped fields above; see
    /// [`Audit::active_policy`](crate::types::Audit::active_policy)
    /// for why audit policy isn't part of the mobile snapshot.
    pub audit_policy: Option<CapturePolicy>,
}

impl UutilsSnapshot {
    pub(crate) fn from_shell(shell: &Shell) -> Self {
        Self {
            env_overrides: shell.mobile.context.env_overrides().clone(),
            dir: shell.mobile.context.dir.clone(),
            grants: shell.mobile.context.grants.clone(),
            location: shell.turn.loc.clone(),
            audit_policy: shell.local.audit.active_policy(),
        }
    }
}

/// Wire form for an already-evaluated redirect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireRedirect {
    pub fd: u32,
    pub mode: RedirectMode,
    pub target: WireRedirectTarget,
}

impl WireRedirect {
    pub(crate) fn from_eval(fd: u32, mode: RedirectMode, target: &command::EvalRedirect) -> Self {
        let target = match target {
            command::EvalRedirect::File(path) => WireRedirectTarget::File(path.clone()),
            command::EvalRedirect::Fd(fd) => WireRedirectTarget::Fd(*fd),
        };
        Self { fd, mode, target }
    }

    fn into_eval(self) -> (u32, RedirectMode, command::EvalRedirect) {
        let target = match self.target {
            WireRedirectTarget::File(path) => command::EvalRedirect::File(path),
            WireRedirectTarget::Fd(fd) => command::EvalRedirect::Fd(fd),
        };
        (self.fd, self.mode, target)
    }
}

/// Wire form for an already-evaluated redirect target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum WireRedirectTarget {
    File(String),
    Fd(u32),
}

/// One typed value crossing a process-staged pipeline boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StageValue {
    pub scope_table: ScopeTable,
    pub value: SerialValue,
}

/// Pack one value boundary for transport between helpers or back to the parent.
pub(crate) fn pack_stage_value(value: &Value) -> Result<StageValue, Error> {
    let mut ctx = InternCtx::new();
    let value = SerialValue::from_runtime(value, &mut ctx).map_err(transfer_error)?;
    Ok(StageValue {
        scope_table: ctx.scope_table,
        value,
    })
}

/// Rehydrate one transported value.
pub(crate) fn unpack_stage_value(value: StageValue) -> Result<Value, Error> {
    let arcs = build_arcs(&value.scope_table)?;
    value.value.into_runtime(&arcs)
}

struct ProcessStateGuard {
    cwd: Option<PathBuf>,
    env: Vec<(String, Option<OsString>)>,
}

impl ProcessStateGuard {
    fn install(ambient: &UutilsSnapshot) -> Settled<Self> {
        // Helper subprocess only: the parent already set our process
        // cwd via `Command::current_dir(shell.cwd())` at spawn time,
        // so this `current_dir()` reads the parent's effective cwd
        // (the within override if any, otherwise the parent's
        // cwd.current).  Saved and re-installed on drop so a uutils
        // tool's internal `chdir` cannot leak across helper jobs.
        #[allow(clippy::disallowed_methods)]
        let cwd = std::env::current_dir().ok();
        let mut env = Vec::new();
        for (key, value) in ambient.env_overrides.iter() {
            // Snapshots the within-override keys to restore on drop; the keys
            // are arbitrary user vars, not basedirs.
            #[allow(clippy::disallowed_methods)]
            env.push((key.clone(), std::env::var_os(key)));
            unsafe {
                std::env::set_var(key, value);
            }
        }
        if let Some(cwd) = &ambient.dir {
            // Helper subprocess only: align the kernel cwd with the
            // parent's `within [dir: …]` override so `uumain`'s
            // `getcwd(3)` matches what ral promised.
            #[allow(clippy::disallowed_methods)]
            std::env::set_current_dir(cwd).map_err(|e| {
                Break::Error(Error::new(format!("pipeline helper: {cwd:?}: {e}"), 1))
            })?;
        }
        Ok(Self { cwd, env })
    }
}

impl Drop for ProcessStateGuard {
    fn drop(&mut self) {
        for (key, value) in self.env.drain(..).rev() {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        if let Some(cwd) = &self.cwd {
            // Helper subprocess only: restore the cwd captured in
            // `install` after `uumain` returns; no parent-side code
            // ever runs through this `Drop`.
            #[allow(clippy::disallowed_methods)]
            let _ = std::env::set_current_dir(cwd);
        }
    }
}

/// Run a terminal bundled-tool job without entering the ral evaluator.
///
/// The uutils arm is not part of the [`run_child_eval`] protocol — no
/// `Comp`, captured env, or mobile crosses it — so it builds its
/// [`ChildEvalResponse`] directly via [`child_eval::report_without_eval`],
/// keeping the report channel a single frame type.
pub(crate) fn run_uutils_stage_job(job: StageJob) -> Settled<ChildEvalResponse> {
    let StageJob::Uutils {
        tool,
        args,
        redirects,
        ambient,
    } = job
    else {
        return Err(Break::Error(Error::new(
            "pipeline helper expected a uutils stage job",
            1,
        )));
    };

    let _guard = ProcessStateGuard::install(&ambient)?;
    let mut shell = Shell::new(Default::default());
    shell.mobile.context.env_overrides = ambient.env_overrides;
    shell.mobile.context.dir = ambient.dir;
    shell.mobile.context.grants = ambient.grants;
    shell.turn.loc = ambient.location;
    shell
        .local
        .audit
        .install_active_policy(ambient.audit_policy);
    shell.turn.io.terminal = TerminalState::probe();
    shell.turn.io.job_control = crate::io::JobControl::pipeline_child();

    let redirects: Vec<_> = redirects.into_iter().map(WireRedirect::into_eval).collect();
    let audit_args: Vec<Value> = args.iter().cloned().map(Value::String).collect();
    let start = audit::start(&shell);
    // This helper builds a fresh Shell whose stdin is a Terminal, so it
    // installs any `<file` redirect itself — `run_external` does the same on
    // the standalone path — and `run_uutils_in_process` binds the parked
    // `Source` onto fd 0 without re-opening it.
    let stdin_guard = command::install_stdin_redirect(&redirects, &mut shell)?;
    let result = command::run_uutils_in_process(&tool, &args, &redirects, &mut shell);
    stdin_guard.restore(&mut shell);
    audit::finish_command(
        &mut shell,
        start,
        &tool,
        &audit_args,
        &result,
        Vec::new(),
        Vec::new(),
    );

    let audit_nodes = shell.local.audit.take_fragment().into_nodes();
    let last_status = shell.mobile.control.last_status;
    report_without_eval(result.map(|_| ()), last_status, audit_nodes)
}

/// Read a required env var and parse it as `T`.  Absent or non-unicode
/// collapse to `"{name} not set"`; present but unparseable becomes
/// `"{name} is not {label}"` (so `label` should read with its article,
/// e.g. `"an fd"`).
fn read_env_required<T>(
    name: &str,
    label: &str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Result<T, String> {
    let s = std::env::var(name).map_err(|_| format!("{name} not set"))?;
    parse(&s).ok_or_else(|| format!("{name} is not {label}"))
}

/// Read an optional env var.  Absent → `Ok(None)`; non-unicode is a
/// structured error; present but unparseable becomes `"{name} is not {label}"`.
fn read_env_optional<T>(
    name: &str,
    label: &str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Result<Option<T>, String> {
    match std::env::var(name) {
        Ok(s) => parse(&s)
            .map(Some)
            .ok_or_else(|| format!("{name} is not {label}")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(format!("{name} is not valid unicode")),
    }
}

/// Helper-side transport for a stage subprocess: turn the inheritable
/// descriptors the parent passed in env vars into the `BufRead` / `Write`
/// channels `serve_stage_core` runs over.
///
/// Both backends do the identical dance — read the job / report
/// descriptors from env, immediately re-secure each against further
/// inheritance, wrap job as a reader and report as a writer, then repeat
/// for the optional value-in / value-out channels.  The only platform
/// difference is the descriptor type (`fd` vs `HANDLE`) and the three
/// primitives below; [`serve_from_env`] is generic over them.
#[cfg(any(unix, windows))]
trait HelperTransport {
    /// Descriptor identity carried through an env var (an fd or a `HANDLE`).
    type Desc: Copy;

    const JOB: &'static str;
    const REPORT: &'static str;
    const VALUE_IN: &'static str;
    const VALUE_OUT: &'static str;

    /// Parse a required descriptor from env var `name`.
    fn read(name: &str) -> Result<Self::Desc, String>;
    /// Parse an optional descriptor from env var `name` (absent → `None`).
    fn read_optional(name: &str) -> Result<Option<Self::Desc>, String>;
    /// Re-secure an inherited descriptor against leaking into the stage's
    /// own children (set `FD_CLOEXEC` / clear `HANDLE_FLAG_INHERIT`).
    fn secure(desc: Self::Desc) -> Result<(), String>;
    /// Wrap a descriptor as a buffered reader.
    fn reader(desc: Self::Desc) -> Box<dyn std::io::BufRead>;
    /// Wrap a descriptor as a writer.
    fn writer(desc: Self::Desc) -> Box<dyn std::io::Write>;
}

#[cfg(any(unix, windows))]
fn serve_from_env<P: HelperTransport>() -> u8 {
    let outcome: Result<u8, ()> = (|| {
        let job = P::read(P::JOB).map_err(report_helper_env_err)?;
        let report = P::read(P::REPORT).map_err(report_helper_env_err)?;
        for d in [job, report] {
            P::secure(d).map_err(report_helper_setup_err)?;
        }
        let mut job_reader = P::reader(job);
        let mut report_writer = P::writer(report);

        let value_in = P::read_optional(P::VALUE_IN).map_err(report_helper_env_err)?;
        let value_out = P::read_optional(P::VALUE_OUT).map_err(report_helper_env_err)?;
        for d in value_in.into_iter().chain(value_out) {
            P::secure(d).map_err(report_helper_setup_err)?;
        }

        Ok(serve_stage_core(
            &mut *job_reader,
            &mut *report_writer,
            value_in.map(P::reader),
            value_out.map(P::writer),
        ))
    })();
    outcome.unwrap_or(1)
}

/// Read exactly one [`StageValue`] frame from `reader`.  EOF is a
/// structured pipeline protocol error — the parent only wires a
/// value-in channel when it expects an upstream value, so a closed
/// pipe means the producer died before reaching its consumer.
fn read_required_stage_value<R: std::io::BufRead>(reader: &mut R) -> Settled<Value> {
    let frame = read_frame::<_, StageValue>(reader).map_err(|e| {
        Break::Error(Error::new(
            format!("pipeline value edge: failed to decode upstream value: {e}"),
            1,
        ))
    })?;
    let value = frame.ok_or_else(|| {
        Break::Error(
            Error::new("pipeline value edge closed before a value was sent", 1)
                .with_hint("did the previous stage fail before returning a value?"),
        )
    })?;
    unpack_stage_value(value).map_err(Break::Error)
}

/// Send one [`StageValue`] over `writer`.  Two failure modes surfaced
/// structurally: non-transferable value (handles etc.) and writer I/O
/// failure (peer closed, EPIPE).
fn write_stage_value<W: std::io::Write>(writer: &mut W, value: &Value) -> Result<(), Error> {
    let packed = pack_stage_value(value)?;
    write_frame(writer, &packed).map_err(|e| {
        Error::new(
            format!("pipeline value edge: failed to send output value: {e}"),
            1,
        )
    })
}

/// Shared stage-serve logic: block-read the [`StageJob`], optionally
/// read an upstream value, run the stage via the shared
/// [`run_child_eval`], optionally forward the output value, and emit the
/// [`ChildEvalResponse`].  Platform-specific I/O setup (fd/handle
/// parsing, CLOEXEC, `from_raw_fd`/`from_raw_handle`) lives in the
/// callers; this function works exclusively through `BufRead` / `Write`.
fn serve_stage_core<R: std::io::BufRead + ?Sized, W: std::io::Write + ?Sized>(
    job_reader: &mut R,
    report_writer: &mut W,
    value_in: Option<Box<dyn std::io::BufRead>>,
    value_out: Option<Box<dyn std::io::Write>>,
) -> u8 {
    let job: StageJob = match read_frame(job_reader) {
        Ok(Some(job)) => job,
        Ok(None) => return 0,
        Err(err) => {
            crate::diagnostic::cmd_error(
                "ral",
                &format!("pipeline helper: failed to decode stage job: {err}"),
            );
            return 1;
        }
    };

    // A stage holding a value-out channel feeds a value consumer, so it
    // forces its output once (`x | f = f !{x}`); a final or byte-mode
    // stage holds none.  Derived here, before `value_out` is consumed by
    // the value-forwarding match below.
    let force_output = value_out.is_some();

    let (report, output_value) = match job {
        StageJob::Ral(request) => {
            let upstream = match value_in {
                Some(mut reader) => match read_required_stage_value(&mut reader) {
                    Ok(value) => Some(value),
                    Err(signal) => {
                        let report = break_response(signal);
                        if let Err(err) = write_frame(report_writer, &report) {
                            crate::diagnostic::cmd_error(
                                "ral",
                                &format!("pipeline helper: failed to write stage report: {err}"),
                            );
                            return 1;
                        }
                        return 0;
                    }
                },
                None => None,
            };
            run_child_eval(request, upstream, ChildKind::PipelineStage { force_output })
        }
        StageJob::Uutils { .. } => {
            if value_in.is_some() || value_out.is_some() {
                (
                    break_response(Break::Error(Error::new(
                        "pipeline helper: uutils stage cannot carry value channels",
                        1,
                    ))),
                    None,
                )
            } else {
                match run_uutils_stage_job(job) {
                    Ok(report) => (report, None),
                    Err(signal) => (break_response(signal), None),
                }
            }
        }
    };

    let report = match (value_out, output_value.as_ref()) {
        (Some(mut writer), Some(value)) => match write_stage_value(&mut writer, value) {
            Ok(()) => report,
            Err(err) => {
                let mut overwritten = break_response(Break::Error(err));
                overwritten.audit_nodes = report.audit_nodes;
                overwritten
            }
        },
        _ => report,
    };

    if let Err(err) = write_frame(report_writer, &report) {
        crate::diagnostic::cmd_error(
            "ral",
            &format!("pipeline helper: failed to write stage report: {err}"),
        );
        return 1;
    }
    0
}

// Helper-side error reporters.  Each consumes a `String` error and
// emits a `cmd_error`, so call sites can `Result::map_err(report_*)?`
// inside an IIFE that propagates `Err(())` to a single `unwrap_or(1)`.
fn report_helper_env_err(err: String) {
    crate::diagnostic::cmd_error("ral", &err);
}
fn report_helper_setup_err(err: String) {
    crate::diagnostic::cmd_error("ral", &format!("pipeline helper: {err}"));
}

#[cfg(unix)]
fn read_env_fd(name: &str) -> Result<i32, String> {
    read_env_required(name, "an fd", |s| s.parse::<i32>().ok())
}

#[cfg(unix)]
struct UnixTransport;

#[cfg(unix)]
impl HelperTransport for UnixTransport {
    type Desc = i32;

    const JOB: &'static str = JOB_FD_ENV;
    const REPORT: &'static str = REPORT_FD_ENV;
    const VALUE_IN: &'static str = VALUE_IN_FD_ENV;
    const VALUE_OUT: &'static str = VALUE_OUT_FD_ENV;

    fn read(name: &str) -> Result<i32, String> {
        read_env_fd(name)
    }

    fn read_optional(name: &str) -> Result<Option<i32>, String> {
        read_env_optional(name, "an fd", |s| s.parse::<i32>().ok())
    }

    fn secure(fd: i32) -> Result<(), String> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }

    fn reader(fd: i32) -> Box<dyn std::io::BufRead> {
        use std::os::fd::FromRawFd;
        use std::os::unix::net::UnixStream;
        Box::new(std::io::BufReader::new(unsafe {
            UnixStream::from_raw_fd(fd)
        }))
    }

    fn writer(fd: i32) -> Box<dyn std::io::Write> {
        use std::os::fd::FromRawFd;
        use std::os::unix::net::UnixStream;
        Box::new(unsafe { UnixStream::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
fn serve_anchor_from_env_fd() -> u8 {
    use std::os::fd::FromRawFd;
    use std::os::unix::net::UnixStream;

    let Ok(fd) = read_env_fd(ANCHOR_FD_ENV).map_err(report_helper_env_err) else {
        return 1;
    };
    let mut stream = unsafe { UnixStream::from_raw_fd(fd) };
    let _ = std::io::copy(&mut stream, &mut std::io::sink());
    0
}

/// Build a helper command that re-execs the current ral binary.
#[cfg(unix)]
pub(crate) fn self_reexec(flag: &str) -> std::io::Result<Command> {
    let mut cmd = crate::sandbox::self_command()?;
    cmd.arg(flag);
    Ok(cmd)
}

/// Build a helper command that re-execs the current ral binary.
///
/// Windows has no sandbox-pinned self-path; we use the live
/// `current_exe`.  The pipeline helpers are short-lived and the
/// child re-enters via the multicall flag, so a swap of the
/// on-disk binary between launch and exec is not a risk we
/// guard against here.
#[cfg(windows)]
pub(crate) fn self_reexec(flag: &str) -> std::io::Result<Command> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg(flag);
    Ok(cmd)
}

// ── Windows helper IO ──────────────────────────────────────────────────────
//
// The Windows transport mirrors the Unix one but takes Win32 `HANDLE`
// values out of env vars (decimal usize) instead of fd numbers, and its
// `secure` clears `HANDLE_FLAG_INHERIT` (the analogue of `FD_CLOEXEC`)
// so the handle does not propagate into nested children spawned during
// stage evaluation.

#[cfg(windows)]
struct WindowsTransport;

#[cfg(windows)]
impl HelperTransport for WindowsTransport {
    type Desc = std::os::windows::io::RawHandle;

    const JOB: &'static str = JOB_HANDLE_ENV;
    const REPORT: &'static str = REPORT_HANDLE_ENV;
    const VALUE_IN: &'static str = VALUE_IN_HANDLE_ENV;
    const VALUE_OUT: &'static str = VALUE_OUT_HANDLE_ENV;

    fn read(name: &str) -> Result<Self::Desc, String> {
        read_env_required(name, "a handle", |s| {
            s.parse::<usize>().ok().map(|v| v as Self::Desc)
        })
    }

    fn read_optional(name: &str) -> Result<Option<Self::Desc>, String> {
        read_env_optional(name, "a handle", |s| {
            s.parse::<usize>().ok().map(|v| v as Self::Desc)
        })
    }

    fn secure(handle: Self::Desc) -> Result<(), String> {
        use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
        let ok = unsafe { SetHandleInformation(handle as HANDLE, HANDLE_FLAG_INHERIT, 0) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }

    fn reader(handle: Self::Desc) -> Box<dyn std::io::BufRead> {
        use std::os::windows::io::FromRawHandle;
        Box::new(std::io::BufReader::new(unsafe {
            os_pipe::PipeReader::from_raw_handle(handle)
        }))
    }

    fn writer(handle: Self::Desc) -> Box<dyn std::io::Write> {
        use std::os::windows::io::FromRawHandle;
        Box::new(unsafe { os_pipe::PipeWriter::from_raw_handle(handle) })
    }
}

/// Hidden helper dispatch from the binary entrypoint.
///
/// Each helper-serving function returns its raw POSIX-style exit code
/// (`127` for exec not-found / I/O, `126` for permission denied, `0`
/// on success, `1` for protocol-layer failures); the binary entrypoint
/// converts to `ExitCode` once.  Collapsing to `0/1` here would discard
/// the documented `126`/`127` distinction the trampoline relies on for
/// `CommandFailure::Spawn` reconstruction at the parent.
pub fn try_run_pipeline_stage_helper() -> Option<u8> {
    let mut args = std::env::args_os();
    let _argv0 = args.next();
    let mode = args.next()?.to_string_lossy().into_owned();
    if mode == RETIRED_EXEC_FLAG {
        eprintln!("ral: pipeline exec helper is no longer used");
        return Some(1);
    }
    #[cfg(unix)]
    {
        crate::sandbox::register_self_for_helpers();
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
        let code = match mode.as_str() {
            HELPER_FLAG => serve_from_env::<UnixTransport>(),
            ANCHOR_FLAG => serve_anchor_from_env_fd(),
            _ => return None,
        };
        Some(code)
    }
    #[cfg(windows)]
    {
        // Windows recognises the stage helper only.  The pgid anchor
        // (`ANCHOR_FLAG`) is Unix-only because the foreground race it
        // exists to address does not exist on Windows.
        match mode.as_str() {
            HELPER_FLAG => Some(serve_from_env::<WindowsTransport>()),
            ANCHOR_FLAG => {
                eprintln!("ral: pipeline helper {mode} is Unix-only; not used on Windows");
                Some(2)
            }
            _ => None,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = mode;
        eprintln!("ral: pipeline helpers are only available on Unix and Windows");
        Some(2)
    }
}

#[cfg(all(test, any(unix, windows), feature = "coreutils"))]
mod tests {
    use super::*;
    use crate::Shell;
    use crate::child_eval::WireOutcome;

    #[test]
    fn uutils_stage_job_runs_terminal_tool_without_ral_eval() {
        let shell = Shell::default();
        let job = StageJob::Uutils {
            tool: "sleep".into(),
            args: vec!["0".into()],
            redirects: Vec::new(),
            ambient: UutilsSnapshot::from_shell(&shell),
        };

        let response = run_uutils_stage_job(job).expect("run");
        assert!(matches!(response.outcome, WireOutcome::Ok(None)));
        assert_eq!(response.last_status, 0);
        assert!(response.mobile.is_none());
    }

    #[test]
    fn uutils_stage_job_nonzero_is_error_not_exit_control() {
        let shell = Shell::default();
        let job = StageJob::Uutils {
            tool: "ls".into(),
            args: vec!["/definitely-not-present-ral-uutils-test".into()],
            redirects: Vec::new(),
            ambient: UutilsSnapshot::from_shell(&shell),
        };

        let response = run_uutils_stage_job(job).expect("run");
        assert!(
            matches!(response.outcome, WireOutcome::Error { status, .. } if status != 0),
            "nonzero uutils status must report ordinary command failure"
        );
        assert_ne!(response.last_status, 0);
    }
}
