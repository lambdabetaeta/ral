//! Hidden multicall flags that let a test drive ral as a pipeline stage and
//! observe runtime state (process group, controlling terminal) without
//! shipping a helper binary.
//!
//! Every one is consumed before clap, so `--help` never names them.

/// Serve a re-exec of `current_exe()` — under `cargo test`, the test binary
/// itself — so a hidden tail flag never reaches libtest's argv parser.
///
/// `Some(code)` means this process was such a re-exec and must exit now.
///
/// Every test binary calls this from a `#[ctor]`; the integration binaries
/// share the one in `core/tests/common/mod.rs`.  `try_run_test_helper` is
/// deliberately absent: its flag is served by the real `ral` binary.
pub fn run_pre_main_reexec_stages() -> Option<u8> {
    #[cfg(unix)]
    crate::uutils::init_signal_dispositions();
    crate::try_run_pipeline_stage_helper(test_prelude())
        .or_else(crate::sandbox::serve_sandbox_early_init)
        .or_else(try_birth_detached)
}

/// The prelude a test binary bakes at runtime, cached for every call this
/// process makes — the pipeline-stage helper and the detach-birth fixture
/// both need one, and there is no build-script blob to embed here.
fn test_prelude() -> &'static crate::boot::BakedPrelude {
    static PRELUDE: std::sync::OnceLock<crate::boot::BakedPrelude> = std::sync::OnceLock::new();
    PRELUDE.get_or_init(crate::boot::BakedPrelude::bake_runtime)
}

/// Tail flag of the detach-birth helper below, re-exec'd by `core/tests/detach.rs`.
pub const DETACH_BIRTH_FLAG: &str = "--ral-test-detach-birth";

/// Serve `--ral-test-detach-birth <trace> <marker>`: birth one `detach` that
/// appends `<marker>` to `<trace>` — and writes it to both its streams, which
/// go nowhere — until killed, print its pid, and exit 1 on a failed birth so
/// the test sees a dead host rather than a missing survivor.
///
/// `detach` promises survival across the *full* exit of its host, so the host
/// must be a process a test can outlive rather than a scope it can leave; the
/// trace is the only sign of a survivor whose streams no one here can name.
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
            test_prelude(),
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
                trail: None,
            },
            surface: None,
            deferred: None,
            desk: None,
            fork: None,
            lifecycle: Box::new(()),
        });
        Some(u8::from(!matches!(
            report,
            crate::RunReport::Ran {
                ending: crate::run::Ending::Settled { .. },
                ..
            }
        )))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Tail flag of the pgid probe below, spelled out literally in `ral/tests/pipeline.rs`.
#[cfg(unix)]
pub(crate) const PGID_CHECK_FLAG: &str = "--ral-test-pgid-check";

/// Serve `--ral-test-pgid-check <tag>`: write `pgid:<tag>=<getpgrp()>` to
/// stderr — plus `tcpgrp:<tag>=<tcgetpgrp(2)>` when stderr is a tty — and
/// exit 0.
///
/// A test can thus confirm a stage joined the pgid its parent set. `None`
/// falls through to the normal CLI.
///
/// The `ral` and `exarch` binaries dispatch this from `main`, because the
/// tests spawn a real `ral` as a pipeline stage.  Stderr is the probe: a
/// stage's stdin and stdout are rerouted through pipes while stderr is
/// inherited, so it alone reports the parent's terminal — a tty under the
/// PTY tests, a pipe under cargo's capture, whatever lies further upstream.
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

        // One buffer, one `write_all`: `eprintln!` emits a syscall per format
        // substitution, and stages sharing a stderr interleave their bytes
        // (`pgid:pgid:downup==…`).  A write under PIPE_BUF is atomic.
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
