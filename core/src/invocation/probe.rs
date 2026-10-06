//! Test roles: hidden flags that let a test birth a detached survivor and
//! observe runtime state (process group, controlling terminal) without
//! shipping a helper binary.

use std::ffi::OsStr;

/// Serve `--ral-test-pgid-check <tag>`: write `pgid:<tag>=<getpgrp()>` to
/// stderr, plus `tcpgrp:<tag>=<tcgetpgrp(2)>` when stderr is a tty, and exit
/// 0.  A test can thus confirm a stage joined the pgid its parent set.
///
/// Stderr is the probe: a stage's stdin and stdout are rerouted through pipes
/// while stderr is inherited, so it alone reports the parent's terminal.
pub(super) fn pgid_check(tag: Option<&OsStr>) -> u8 {
    use std::fmt::Write as _;
    use std::io::{IsTerminal, Write};

    let tag = tag.map_or_else(|| "stage".into(), OsStr::to_string_lossy);
    let stderr = std::io::stderr();
    // One buffer, one `write_all`: stages sharing a stderr interleave their
    // bytes per format substitution, and a write under PIPE_BUF is atomic.
    let mut buf = String::new();
    let _ = writeln!(
        buf,
        "pgid:{tag}={}",
        rustix::process::getpgrp().as_raw_nonzero()
    );
    if stderr.is_terminal()
        && let Ok(fg) = rustix::termios::tcgetpgrp(&stderr)
    {
        let _ = writeln!(buf, "tcpgrp:{tag}={}", fg.as_raw_nonzero());
    }
    let _ = stderr.lock().write_all(buf.as_bytes());
    // A reader stage that exits before its writer is done makes ral end that
    // writer (SPEC §7.6), so the probe holds its stdin open to EOF rather than
    // racing its own upstream's report.  A tty never sees EOF.
    if !std::io::stdin().is_terminal() {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
    }
    0
}

/// Serve `--ral-test-detach-birth <trace> <marker>`: birth one `detach` that
/// appends `<marker>` to `<trace>`, and writes it to both its streams, which
/// go nowhere, until killed; print its pid; exit 1 on a failed birth so the
/// test sees a dead host rather than a missing survivor.
///
/// `detach` promises survival across the full exit of its host, so the host
/// must be a process a test can outlive; the trace is the only sign of a
/// survivor whose streams no one here can name.
pub(super) fn detach_birth(trace: &OsStr, marker: &OsStr) -> u8 {
    use crate::boot::{BakedPrelude, HostSurface, boot_shell};
    use crate::protocol::Run;
    use crate::run::{Ending, RunReport};

    // The survivor must inherit SIGPIPE's default, as a shell's children do.
    crate::uutils::init_signal_dispositions();
    let (trace, marker) = (trace.to_string_lossy(), marker.to_string_lossy());
    let mut shell = boot_shell(
        crate::terminal::TerminalState::default(),
        BakedPrelude::runtime(),
        &HostSurface::default(),
    );
    shell.install_builtins(crate::builtins::DETACH_BUILTIN);
    shell.arm_detach(1);
    let report = shell.run(Run::foreground(
        format!(
            "let d = detach #'the survivor a test outlives'# \
                 /bin/sh -c 'while :; do echo {marker}; echo {marker} >&2; \
                 echo {marker} >> {trace}; sleep 0.05; done'; echo $d[pid]"
        ),
        "<detach-birth>",
    ));
    u8::from(!matches!(
        report,
        RunReport::Ran {
            ending: Ending::Settled { .. },
            ..
        }
    ))
}
