//! Hidden helper modes for ral's own integration tests.
//!
//! These flags exist so a test can drive ral as a pipeline stage and
//! observe runtime state (process group, controlling terminal) without
//! shipping an extra helper binary.  The flags are not user-facing and
//! are not advertised in `--help`.
//!
//! [`run_pre_main_reexec_stages`] is the pre-`main` body every test binary
//! (the lib unit tests and the `core/tests/*.rs` integration binaries)
//! runs from its `#[ctor]`.

/// Run the pre-`main` re-exec dispatch shared by ral's test binaries.
///
/// A test that spawns a pipeline stage or a per-command sandbox re-execs
/// `current_exe()` — which under `cargo test` is the test binary itself —
/// with a hidden tail flag.  Two stages are dispatched here, chained by
/// `.or_else` so a non-re-exec invocation falls through:
///
///   1. `try_run_pipeline_stage_helper` — `--ral-pipeline-stage-helper`
///      and friends serve one pipeline / capture stage.
///   2. `serve_sandbox_early_init` — `--sandbox-projection` enters the OS
///      sandbox, then runs the confined `--ral-sandbox-exec` host binary
///      or `--ral-bundled-tool` tool.
///   3. [`try_birth_detached`] — [`DETACH_BIRTH_FLAG`] births one `detach`
///      and exits, so a test can watch what survives a host that is gone.
///
/// Returns `Some(code)` when this process was such a re-exec and must exit
/// immediately; `None` for a normal test run, which then reaches libtest.
pub fn run_pre_main_reexec_stages() -> Option<u8> {
    #[cfg(unix)]
    crate::builtins::uutils::init_signal_dispositions();
    crate::try_run_pipeline_stage_helper()
        .or_else(crate::sandbox::serve_sandbox_early_init)
        .or_else(try_birth_detached)
}

/// Hidden multicall sentinel: birth one detached process and exit at once.
pub const DETACH_BIRTH_FLAG: &str = "--ral-test-detach-birth";

/// Serve `--ral-test-detach-birth <trace> <marker>`: arm this shell's
/// `detach` authority with a budget of one, birth a process that appends to
/// `<trace>` — and writes `<marker>` to both its streams, which go nowhere —
/// until killed, print its pid, and exit.
///
/// The property `detach` exists for is survival across the *full* exit of
/// the process that birthed it, so the host has to be a whole process a
/// test can outlive rather than a scope it can leave.  A test re-execs this
/// binary here, waits for it to be gone, and then reads the pid off the
/// host's own stdout: the survivor writes nothing this session can name, so
/// the trace file it keeps for itself is the only sign it is still working.
///
/// Exits 1 if the birth failed, so the test sees a dead host rather than a
/// missing survivor.
fn try_birth_detached() -> Option<u8> {
    #[cfg(unix)]
    {
        let mut args = std::env::args_os().skip(1);
        if args.next()? != DETACH_BIRTH_FLAG {
            return None;
        }
        let (trace, marker) = (
            args.next()?.to_string_lossy().into_owned(),
            args.next()?.to_string_lossy().into_owned(),
        );
        let mut shell = crate::boot::boot_shell(
            crate::io::TerminalState::default(),
            &crate::boot::BakedPrelude::bake_runtime(),
            &crate::boot::HostSurface::default(),
        );
        shell.install_builtins(crate::builtins::DETACH_BUILTIN);
        shell.arm_detach(1);
        let report = shell.run(crate::RunRequest {
            run: crate::transport::Run {
                program: crate::transport::Program::Source(format!(
                    "let d = detach #'the survivor a test outlives'# \
                     /bin/sh -c 'while :; do echo {marker}; echo {marker} >&2; \
                     echo {marker} >> {trace}; sleep 0.05; done'; echo $d[pid]"
                )),
                script_name: "<detach-birth>".into(),
                caps: crate::types::Capabilities::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: crate::RunIo::Inherit,
                terminal: crate::RequestedTerminalAccess::Leased,
                stdin: crate::RunStdin::Inherit,
            },
            surface: None,
            deferred: None,
            desk: None,
            nursery: None,
            lifecycle: Box::new(()),
        });
        Some(u8::from(!matches!(
            report,
            crate::RunReport::Ran { result: Ok(_), .. }
        )))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Hidden multicall sentinel: emit the helper's own pgid and exit.
#[cfg(unix)]
pub(crate) const PGID_CHECK_FLAG: &str = "--ral-test-pgid-check";

/// Dispatch a hidden test-helper flag.  Returns `Some(code)` when the
/// flag was recognised so the caller can exit immediately; otherwise
/// `None` and the normal CLI path runs.
///
/// `--ral-test-pgid-check <tag>`
///     Write `pgid:<tag>=<getpgrp()>\n` to stderr and exit 0.  Tests use
///     this to confirm pipeline stages share the pgid the parent
///     established.  When *stderr* is a tty the helper additionally
///     writes `tcpgrp:<tag>=<tcgetpgrp(stderr)>\n`.  Stderr is the
///     pipeline-friendly probe: pipeline stages have their stdin /
///     stdout rerouted through OS pipes but inherit stderr from the
///     parent ral, so PTY-based tests (which install the pty as the
///     parent's stderr) see "tty=yes" while batch-mode tests (cargo's
///     piped stderr capture) see "tty=no" — even when a controlling
///     terminal exists upstream of the cargo runner.
pub fn try_run_test_helper() -> Option<u8> {
    #[cfg(unix)]
    {
        use std::fmt::Write as _;
        use std::io::{IsTerminal, Write};

        let mut args = std::env::args_os();
        let _argv0 = args.next();
        let flag = args.next()?;
        if flag != PGID_CHECK_FLAG {
            return None;
        }
        let tag = args
            .next()
            .map_or_else(|| "stage".into(), |t| t.to_string_lossy().into_owned());
        let pgid = unsafe { libc::getpgrp() };
        let stderr_fd = libc::STDERR_FILENO;

        // Build the whole output in one buffer and emit it via a
        // single `write_all`.  Bare `eprintln!` produces several small
        // syscalls per format substitution; with three pipeline
        // stages writing the same line concurrently to a shared
        // stderr the bytes interleave (`pgid:pgid:downup==…`).  A
        // single `write_all` under PIPE_BUF (4096) is atomic on
        // pipes and ttys.
        let mut buf = String::new();
        let _ = writeln!(&mut buf, "pgid:{tag}={pgid}");
        if std::io::stderr().is_terminal() {
            let fg = unsafe { libc::tcgetpgrp(stderr_fd) };
            if fg >= 0 {
                let _ = writeln!(&mut buf, "tcpgrp:{tag}={fg}");
            }
        }
        let _ = std::io::stderr().write_all(buf.as_bytes());
        Some(0)
    }
    #[cfg(not(unix))]
    {
        None
    }
}
