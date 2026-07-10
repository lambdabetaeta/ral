//! Hidden helper modes for process-staged pipelines.
//!
//! `--ral-pipeline-stage-helper` runs one ral stage in a fresh child
//! shell.  The parent packs the stage and the ambient shell snapshot
//! into a [`ChildEvalRequest`]; the helper reconstructs the shell,
//! optionally reads one typed upstream value from `VALUE_IN_FD_ENV`, runs
//! the stage via `invoke`, and emits a structured
//! [`ChildEvalResponse`](crate::child_eval::ChildEvalResponse) (with the
//! final value embedded for the parent to recover after the helper exits).
//!
//! `--ral-pipeline-anchor` owns the pipeline pgid for the whole launch
//! so a fast-exiting first stage cannot strand later stages.

use crate::child_eval::{ChildEvalRequest, break_response, run_child_eval, transfer_error};
use crate::serial::{InternCtx, ScopeTable, SerialValue, build_arcs};
use crate::subprocess_codec::{read_frame, write_frame};
use crate::types::{Break, Error, Settled, Value};
use serde::{Deserialize, Serialize};

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

/// Hidden multicall sentinel for a child placement of a bundled tool
/// (`ral --ral-bundled-tool <tool> <args...>`).  Unlike the stage helper
/// this child reads no [`ChildEvalRequest`] and sends no
/// [`ChildEvalResponse`](crate::child_eval::ChildEvalResponse): its
/// inherited env/cwd/stdio/process-group/sandbox are the execution
/// context, so it just runs `uutils_invoke` and exits with the tool
/// status.  See `decisions/260616_bundled-tools-as-exec-images`.
pub(crate) const BUNDLED_TOOL_FLAG: &str = "--ral-bundled-tool";

/// Optional start-gate descriptor for a bundled-tool child.
///
/// No live launcher wires this today: direct bundled tools are admitted
/// only on the same no-helper path as host externals, so they run
/// immediately.  If a future direct bundled-tool gate is needed, the
/// child side already has the same shape as the anchor gate: read this
/// descriptor to EOF, then enter the tool.
#[cfg(all(
    unix,
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
pub(crate) const BUNDLED_TOOL_GATE_FD_ENV: &str = "RAL_BUNDLED_TOOL_GATE_FD";
#[cfg(all(
    windows,
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
pub(crate) const BUNDLED_TOOL_GATE_HANDLE_ENV: &str = "RAL_BUNDLED_TOOL_GATE_HANDLE";

/// A multicall sentinel that no live caller emits.  Recognising it here
/// turns a stale invocation — an old script, or a re-exec from a
/// mismatched binary — into a clear diagnostic and a non-zero exit
/// rather than an opaque clap usage error.
pub(crate) const RETIRED_EXEC_FLAG: &str = "--ral-pipeline-exec-helper";

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

/// Shared stage-serve logic: block-read the [`ChildEvalRequest`],
/// optionally read an upstream value, run the stage via the shared
/// [`run_child_eval`], optionally forward the output value, and emit the
/// [`ChildEvalResponse`](crate::child_eval::ChildEvalResponse).
/// Platform-specific I/O setup (fd/handle parsing, CLOEXEC,
/// `from_raw_fd`/`from_raw_handle`) lives in the callers; this function
/// works exclusively through `BufRead` / `Write`.
fn serve_stage_core<R: std::io::BufRead + ?Sized, W: std::io::Write + ?Sized>(
    job_reader: &mut R,
    report_writer: &mut W,
    value_in: Option<Box<dyn std::io::BufRead>>,
    value_out: Option<Box<dyn std::io::Write>>,
) -> u8 {
    let request: ChildEvalRequest = match read_frame(job_reader) {
        Ok(Some(request)) => request,
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
    let (report, output_value) = run_child_eval(request, upstream, force_output);

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
pub(crate) fn self_reexec(flag: &str) -> std::io::Result<crate::process::Launch> {
    let mut cmd = crate::sandbox::self_command()?;
    cmd.arg(flag);
    Ok(crate::process::Launch::from_command(cmd))
}

/// Build a helper command that re-execs the current ral binary.
///
/// Windows has no sandbox-pinned self-path; we use the live
/// `current_exe`.  The pipeline helpers are short-lived and the
/// child re-enters via the multicall flag, so a swap of the
/// on-disk binary between launch and exec is not a risk we
/// guard against here.
#[cfg(windows)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:self-reexec-windows] Builds the ral-re-exec Command for Windows pipeline-helper / bundled-tool multicall subprocesses. Infrastructure spawn, not a model exec image — the model's exec surfaces at command::run, not here."
)]
pub(crate) fn self_reexec(flag: &str) -> std::io::Result<crate::process::Launch> {
    let exe = std::env::current_exe()?;
    let mut cmd = crate::process::Launch::new(exe);
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

/// Block until an optional bundled-tool start gate reaches EOF.
///
/// No current launcher sets the env var, so bundled-tool children run
/// immediately.  The reader remains the child half of the dormant gate
/// protocol: if a parent later passes one inheritable descriptor, closing
/// the parent end releases the tool.
#[cfg(all(
    unix,
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
fn block_on_bundled_tool_gate() {
    use std::os::fd::FromRawFd;
    use std::os::unix::net::UnixStream;
    match read_env_optional(BUNDLED_TOOL_GATE_FD_ENV, "an fd", |s| s.parse::<i32>().ok()) {
        Ok(Some(fd)) => {
            let mut stream = unsafe { UnixStream::from_raw_fd(fd) };
            let _ = std::io::copy(&mut stream, &mut std::io::sink());
        }
        Ok(None) => {}
        Err(err) => report_helper_env_err(err),
    }
}

#[cfg(all(
    windows,
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
fn block_on_bundled_tool_gate() {
    use std::os::windows::io::FromRawHandle;
    match read_env_optional(BUNDLED_TOOL_GATE_HANDLE_ENV, "a handle", |s| {
        s.parse::<usize>()
            .ok()
            .map(|v| v as std::os::windows::io::RawHandle)
    }) {
        Ok(Some(handle)) => {
            let mut reader = unsafe { os_pipe::PipeReader::from_raw_handle(handle) };
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        }
        Ok(None) => {}
        Err(err) => report_helper_env_err(err),
    }
}

/// There is no inheritable-descriptor gate transport off Unix/Windows;
/// the bundled tools only link on those platforms anyway (see
/// `run_uutils_in_process`), so the gate is a no-op here.
#[cfg(all(
    not(any(unix, windows)),
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
fn block_on_bundled_tool_gate() {}

/// Hidden bundled-tool dispatch from the binary entrypoint
/// (`ral --ral-bundled-tool <tool> <args...>`).
///
/// Returns `None` when `args` does not start with [`BUNDLED_TOOL_FLAG`]
/// (the normal CLI path runs); otherwise runs the bundled tool in this
/// process and returns `Some(exit_code)`.
///
/// `args` is the post-`early_init` argv slice (sans the binary name) — by
/// then the OS sandbox is already entered, so the tool runs confined.
/// The child reads no `ChildEvalRequest` and emits no `ChildEvalResponse`:
/// its inherited env/cwd/stdio/process-group/sandbox are the execution
/// context.  The exit code combines `uutils_invoke`'s direct return with
/// uucore's process-global cell exactly as `run_uutils_in_process` does
/// (`if global == 0 { code } else { global }`).
#[cfg(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep"))]
pub fn try_run_bundled_tool(args: &[String]) -> Option<u8> {
    use crate::builtins::uutils;

    let (flag, rest) = args.split_first()?;
    if flag != BUNDLED_TOOL_FLAG {
        return None;
    }
    let Some((tool, tool_args)) = rest.split_first() else {
        crate::diagnostic::cmd_error("ral", &format!("{BUNDLED_TOOL_FLAG} requires a tool name"));
        return Some(2);
    };
    if !uutils::is_uutils_tool(tool) {
        crate::diagnostic::cmd_error("ral", &format!("'{tool}' is not a bundled tool"));
        return Some(127);
    }

    block_on_bundled_tool_gate();

    // Slot 0 carries the tool name for every tool — `uutils_invoke`'s `rg`
    // arm drops it internally (`ral-ripgrep-core` wants argv without
    // argv[0]); coreutils/diffutils keep it.  This matches exactly how
    // `run_uutils_in_process` builds `os_args`, so there is no per-family
    // branching here and no double-adjustment of ripgrep's argv.
    let os_args: Vec<std::ffi::OsString> = std::iter::once(std::ffi::OsString::from(tool.as_str()))
        .chain(tool_args.iter().map(std::ffi::OsString::from))
        .collect();

    uutils::reset_exit_code();
    let code = uutils::uutils_invoke(tool, os_args);
    let global = uutils::get_exit_code();
    let exit_code = if global == 0 { code } else { global };
    #[allow(clippy::cast_sign_loss, reason = "clamp(0, 255) bounds the value to the u8 range before the cast")]
    Some(exit_code.clamp(0, 255) as u8)
}

/// No-bundled-features fallback so the binary entrypoint stays
/// feature-clean.
///
/// Without any bundled tool linked in there is nothing
/// `--ral-bundled-tool` could dispatch, so it is unreachable; recognise
/// the sentinel anyway to turn it into a clear diagnostic rather than an
/// opaque clap usage error, mirroring [`RETIRED_EXEC_FLAG`].
#[cfg(not(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")))]
pub fn try_run_bundled_tool(args: &[String]) -> Option<u8> {
    let flag = args.first()?;
    if flag != BUNDLED_TOOL_FLAG {
        return None;
    }
    crate::diagnostic::cmd_error("ral", "no bundled tools are available in this build");
    Some(127)
}
