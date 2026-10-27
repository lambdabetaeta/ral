include!("../../lint/site.rs");

/// Host plumbing has no shell or model run in which to raise a card.
#[derive(Clone, Copy)]
pub(crate) enum Silent {
    /// Resolve the granted folder before any engine exists; no shell handles host setup.
    GrantedFolderResolve,
    /// Host setup checks the resolved folder; no engine or model run exists.
    GrantedFolderStat,
    /// Resolve boot files before any engine exists; no shell handles host setup.
    BootMediaResolve,
    /// Inflation and locked publication in the image cache, before any engine exists.
    RootfsInflate,
    /// Create the session disk before any engine exists; host setup raises no card.
    #[cfg(any(test, target_os = "macos", windows))]
    SessionDiskCreate,
    /// Delete the disk after power-off, retrying held handles; host teardown raises no card.
    #[cfg(any(test, target_os = "macos", windows))]
    SessionDiskRelease,
    /// Host temp configuration supplies macOS session storage; reading it opens no object.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    SessionDiskFolder,
    /// Poison refuses the connection without spending the announce; the guest may redial. Guards memory only.
    #[cfg(target_os = "macos")]
    AnnounceLock,
    /// Windows' own cache location is path-valued; reading it opens no object.
    #[cfg(windows)]
    LocalAppDataVar,
    /// Without `LOCALAPPDATA`, use host temp configuration; reading it opens no object.
    #[cfg(windows)]
    LocalAppDataFallback,
    /// JSON paths are strict Unicode, never approximated; conversion opens no object.
    #[cfg(windows)]
    DocumentText,
    /// Measure our session disk for host diagnostics; no shell or model run exists.
    #[cfg(any(test, target_os = "macos", windows))]
    SessionDiskMeasure,
    /// Reclaim only abandoned session disks; housekeeping precedes any guest.
    #[cfg(any(test, target_os = "macos", windows))]
    SessionDiskSweep,
    /// Only host stderr asides choose their own spelling; displaying a path opens nothing.
    #[cfg(any(test, target_os = "macos", windows))]
    AsideSpelling,
    /// Poison cannot erase earlier console words; the lock guards memory only.
    #[cfg(windows)]
    ConsoleRingLock,
    /// Open our diagnostic log before any guest exists; no shell handles host setup.
    #[cfg(windows)]
    ConsoleLogOpen,
    /// Sweep only our stale console logs; this is host diagnostic housekeeping.
    #[cfg(windows)]
    ConsoleLogSweep,
    /// Discard only our own diagnostic log; this is host housekeeping.
    #[cfg(windows)]
    ConsoleLogDiscard,
    /// Connect without read/write access solely to wake our pump; no contents are opened.
    #[cfg(windows)]
    ConsolePipeWake,
    /// The shipped image's metadata for its wrap identity; host setup before any engine.
    #[cfg(windows)]
    RootfsWrapProbe,
    /// The source opened for an unpublished VHD stage, before any engine exists.
    #[cfg(windows)]
    RootfsWrap,
    /// The client end of the service's own pipe, retried while busy; host-side, before any machine.
    #[cfg(windows)]
    BrokerPipeConnect,
    /// The executable's installation and checkout layout, which no grant judges.
    CheckoutRoot,
    /// Probes and checksum reads of installed media, before any engine exists.
    BootMediaProbe,
}

impl Site for Silent {}
