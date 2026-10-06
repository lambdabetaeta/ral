//! The pipeline anchor, a re-exec of ral in `Role::Anchor`: it holds the
//! pipeline pgid open so a fast-exiting first stage cannot strand its
//! successors.  Its byte protocol is read by `AnchorProcess` in `group.rs`.

/// Serve the pipeline anchor: block reading stdin to EOF — the parent's
/// `AnchorProcess::finish` closing its release pipe.  It spawns nothing, so
/// it pins nothing.  SIGPIPE is at its default; every termination signal is
/// swallowed and reported instead; the three stop signals are ignored
/// outright, so the anchor never stops and never needs resuming.
#[cfg(unix)]
pub(crate) fn serve() -> u8 {
    crate::uutils::init_signal_dispositions();
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        unsafe {
            libc::signal(sig, report_signal as *const () as libc::sighandler_t);
        }
    }
    for sig in [libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
        unsafe {
            libc::signal(sig, libc::SIG_IGN);
        }
    }
    let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
    0
}

/// Async-signal-safe: one `write(2)` of the signal number to stdout.
#[cfg(unix)]
extern "C" fn report_signal(sig: libc::c_int) {
    let byte = [u8::try_from(sig).unwrap_or(0)];
    unsafe {
        libc::write(libc::STDOUT_FILENO, byte.as_ptr().cast(), 1);
    }
}

/// Serve the pipeline anchor: block reading stdin to EOF.  Every console
/// event is swallowed, Ctrl-Break included: the group's grace is for its
/// stages, never the anchor holding the group open.
#[cfg(windows)]
pub(crate) fn serve() -> u8 {
    extern "system" fn swallow(_: u32) -> windows_sys::core::BOOL {
        windows_sys::Win32::Foundation::TRUE
    }
    // SAFETY: `swallow` has `PHANDLER_ROUTINE`'s signature; `TRUE` adds it.
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(swallow), 1);
    }
    let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
    0
}
