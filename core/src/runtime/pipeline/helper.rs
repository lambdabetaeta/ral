//! Child side of ral's hidden multicall flags, entered before the CLI.
//!
//! `--ral-pipeline-stage-helper` rebuilds a shell from the
//! [`ChildEvalRequest`] the parent writes into the job channel, optionally
//! reads one upstream value, runs the stage, and writes one response frame
//! back — the final value rides inside it.  `--ral-pipeline-anchor` holds
//! the pipeline pgid open so a fast-exiting first stage cannot strand its
//! successors.  `--ral-bundled-tool` exchanges no frame at all: the
//! inherited env, cwd, stdio, process group and sandbox are the whole
//! execution context.

use crate::child_eval::{ChildEvalRequest, break_response, run_child_eval, transfer_error};
use crate::serial::{InternCtx, ScopeTable, SerialValue, build_arcs};
use crate::subprocess_codec::{read_frame, write_frame};
use crate::types::{Break, Error, Settled, Value};
use serde::{Deserialize, Serialize};

pub(crate) const HELPER_FLAG: &str = "--ral-pipeline-stage-helper";

// The parent fills these: stage channels in `protocol/{unix,windows}.rs`,
// the anchor's in `group.rs`.
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

pub(crate) const ANCHOR_FLAG: &str = "--ral-pipeline-anchor";

pub(crate) const BUNDLED_TOOL_FLAG: &str = "--ral-bundled-tool";

/// One typed value crossing a process-staged pipeline boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StageValue {
    scope_table: ScopeTable,
    value: SerialValue,
}

fn pack_stage_value(value: &Value) -> Result<StageValue, Error> {
    let mut ctx = InternCtx::new();
    let value = SerialValue::from_runtime(value, &mut ctx).map_err(|e| transfer_error(&e))?;
    Ok(StageValue {
        scope_table: ctx.scope_table,
        value,
    })
}

fn unpack_stage_value(value: StageValue) -> Result<Value, Error> {
    let arcs = build_arcs(&value.scope_table)?;
    value.value.into_runtime(&arcs)
}

/// `label` lands in `"{name} is not {label}"`, so it carries its own
/// article: `"an fd"`.
fn read_env_required<T>(
    name: &str,
    label: &str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Result<T, String> {
    let s = std::env::var(name).map_err(|_| format!("{name} not set"))?;
    parse(&s).ok_or_else(|| format!("{name} is not {label}"))
}

/// As `read_env_required`, but an unset var is `Ok(None)` rather than an error.
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

/// All that separates the two backends: the descriptor type and five
/// primitives, so `serve_from_env` performs one dance on both.
trait HelperTransport {
    /// An fd on Unix, a Win32 `HANDLE` on Windows.
    type Desc: Copy;

    const JOB: &'static str;
    const REPORT: &'static str;
    const VALUE_IN: &'static str;
    const VALUE_OUT: &'static str;

    fn read(name: &str) -> Result<Self::Desc, String>;
    fn read_optional(name: &str) -> Result<Option<Self::Desc>, String>;
    /// Stop the inherited descriptor leaking on into the stage's own
    /// children (set `FD_CLOEXEC` / clear `HANDLE_FLAG_INHERIT`).
    fn secure(desc: Self::Desc) -> Result<(), String>;
    fn reader(desc: Self::Desc) -> Box<dyn std::io::BufRead>;
    fn writer(desc: Self::Desc) -> Box<dyn std::io::Write>;
}

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

/// EOF is an error, not an empty read: the parent wires a value-in channel
/// only when a value is coming, so a closed pipe means the producer died
/// before reaching its consumer.
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

/// A value that cannot be serialized — an open handle, say — fails here
/// rather than at the peer.
fn write_stage_value<W: std::io::Write>(writer: &mut W, value: &Value) -> Result<(), Error> {
    let packed = pack_stage_value(value)?;
    write_frame(writer, &packed).map_err(|e| {
        Error::new(
            format!("pipeline value edge: failed to send output value: {e}"),
            1,
        )
    })
}

/// One job frame in, one report frame out; the transport has already turned
/// every descriptor into a stream.
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

    // A value-out channel means a value consumer downstream, so the output
    // is forced once (`x | f = f !{x}`).  Read before the match below moves it.
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

// Point-free `map_err` targets: each reports and yields `()`, so the IIFE
// in `serve_from_env` can carry every failure to one `unwrap_or(1)`.
#[allow(
    clippy::needless_pass_by_value,
    reason = "point-free `map_err` target: takes the error String flowing off the fallible edge"
)]
fn report_helper_env_err(err: String) {
    crate::diagnostic::cmd_error("ral", &err);
}
#[allow(
    clippy::needless_pass_by_value,
    reason = "point-free `map_err` target: takes the error String flowing off the fallible edge"
)]
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
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        let flags = rustix::io::fcntl_getfd(borrowed).map_err(|e| e.to_string())?;
        rustix::io::fcntl_setfd(borrowed, flags | rustix::io::FdFlags::CLOEXEC)
            .map_err(|e| e.to_string())
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
/// Windows has no sandbox-pinned self-path, so this takes the live
/// `current_exe` and would follow an on-disk swap between launch and exec.
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

/// Hidden helper dispatch from the binary entrypoint; `None` when argv names
/// no helper mode and the ordinary CLI should run.
///
/// A stage's own errors ride home in the report frame, so the exit code is
/// only ever `0`, or `1` for a failure below the protocol.
pub fn try_run_pipeline_stage_helper() -> Option<u8> {
    let mut args = std::env::args_os();
    let _argv0 = args.next();
    let mode = args.next()?.to_string_lossy().into_owned();
    #[cfg(unix)]
    {
        crate::sandbox::register_self_for_helpers();
        // Before the mode match, so every argv-bearing ral gets it: Rust's
        // runtime ignores SIGPIPE, but a pipeline producer must die when its
        // reader closes (`yes | head`).  Parent-side protocol writes, which
        // need `EPIPE` instead, mask it per write in `subprocess_codec`.
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
        // The anchor is Unix-only: Windows has no pgid for a fast first
        // stage to strand.
        match mode.as_str() {
            HELPER_FLAG => Some(serve_from_env::<WindowsTransport>()),
            ANCHOR_FLAG => {
                eprintln!("ral: pipeline helper {mode} is Unix-only; not used on Windows");
                Some(2)
            }
            _ => None,
        }
    }
}

/// Hidden bundled-tool dispatch from the binary entrypoint
/// (`ral --ral-bundled-tool <tool> <args...>`).
///
/// `args` is the post-`early_init` argv sans the binary name, so the OS
/// sandbox is already entered and the tool runs confined.  The exit code
/// comes from `invoke_bundled`, shared with the inline
/// `run_uutils_in_process` placement.
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

    let exit_code = uutils::invoke_bundled(tool, tool_args);
    #[allow(
        clippy::cast_sign_loss,
        reason = "clamp(0, 255) bounds the value to the u8 range before the cast"
    )]
    Some(exit_code.clamp(0, 255) as u8)
}

/// With no bundled tool linked in the sentinel is unreachable, but still
/// recognised, so it fails as a clear diagnostic rather than a clap usage
/// error.
#[cfg(not(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")))]
pub fn try_run_bundled_tool(args: &[String]) -> Option<u8> {
    let flag = args.first()?;
    if flag != BUNDLED_TOOL_FLAG {
        return None;
    }
    crate::diagnostic::cmd_error("ral", "no bundled tools are available in this build");
    Some(127)
}
