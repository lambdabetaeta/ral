// The typed-site vocabulary of this crate: see `lint/site.rs`.
include!("../../lint/site.rs");

/// Sites that raise no card.
///
/// This crate links no ral-core: its calls are host-side plumbing around a
/// machine, with no shell to route them through and no run to raise a card in.
#[derive(Clone, Copy)]
pub(crate) enum Silent {
    /// The canonicalisation of the folder a machine is granted, resolved
    /// before a machine exists.  It raises no card: this runs on the host
    /// before any engine, and vm-manager deliberately does not depend on
    /// ral-core's path layer.
    GrantedFolderResolve,
    /// The stat of the folder just resolved, to see that it is a directory:
    /// the same host-side resolution, one stat further.  It raises no card,
    /// for the reason of `GrantedFolderResolve`.
    GrantedFolderStat,
    /// The canonicalisation of each boot file (kernel, initramfs, rootfs)
    /// before a machine exists, exactly as the granted folder is resolved.  It
    /// raises no card: host-side setup with no shell to route through.
    BootMediaResolve,
    /// The inflation of the shipped rootfs archive: the archive opened, its
    /// `.part` sibling created, renamed into place, or removed after a
    /// failure.  It raises no card: host-side disk plumbing before any engine
    /// exists.
    RootfsInflate,
    /// The creation of a session's own read-write disk, and on Windows the
    /// directory it goes in.  It raises no card: this is host-side session
    /// setup before any engine exists, not a model's turn-time write.
    #[cfg(any(windows, target_os = "macos"))]
    SessionDiskCreate,
    /// The deletion of a session's disk once its machine is powered off,
    /// retried on Windows while the worker process still holds it.  It raises
    /// no card: infrastructure teardown of a file this crate made, not a
    /// model's turn-time write.
    #[cfg(any(windows, target_os = "macos"))]
    SessionDiskRelease,
    /// The host's temp directory, handed to the macOS backend that
    /// [`detect`](crate::detect) builds as the folder to keep each session's
    /// read-write disk in: the application names no other place.  This crate
    /// has no ral-core to ask for it.  It raises no card: it reads the host's
    /// own configuration and opens no object.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    SessionDiskFolder,
    /// The lock on the one-shot announce a guest's first connection is
    /// reported through, answering poison by hand: a poisoned announce refuses
    /// the connection, the same benign path a failed duplicate takes, so the
    /// announce stays unspent and the guest daemon redials.  It raises no
    /// card: it guards memory.
    #[cfg(target_os = "macos")]
    AnnounceLock,
    /// `%LOCALAPPDATA%`, lifted as one path-valued environment variable for the
    /// backend's default machine cache.  It is Windows' own cache location,
    /// not an XDG basedir read, and this crate has no ral-core basedir layer to
    /// ask.  It raises no card: reading a variable opens no object.
    #[cfg(windows)]
    LocalAppDataVar,
    /// The host's temp directory, where `%LOCALAPPDATA%` is unset: the
    /// backend's default machine cache then has no other place to go.  This
    /// crate has no ral-core to ask for it.  It raises no card: it reads the
    /// host's own configuration and opens no object.
    #[cfg(windows)]
    LocalAppDataFallback,
    /// `%ProgramData%`, lifted as one path-valued environment variable for the
    /// machine service's machine-wide state directory, which a `LocalSystem`
    /// service cannot take from a profile.  It raises no card: reading a
    /// variable opens no object.
    #[cfg(windows)]
    ProgramDataVar,
    /// The host's temp directory, where `%ProgramData%` is unset: the machine
    /// service's machine-wide state then has no other place to go.  This crate
    /// has no ral-core to ask for it.  It raises no card: it reads the host's
    /// own configuration and opens no object.
    #[cfg(windows)]
    ProgramDataFallback,
    /// A path as the JSON text a machine's document carries it in, converted
    /// strictly so that a name that is not Unicode is refused and never
    /// approximated.  It is the name the compute service opens, and is never
    /// shown; it raises no card: no object is opened.
    #[cfg(windows)]
    DocumentText,
    /// The measurement of a session disk this backend made, to say in a
    /// diagnostic how much of someone's disk it is holding.  It raises no
    /// card: a stat of a file this crate made, with no shell and no run.
    #[cfg(windows)]
    SessionDiskMeasure,
    /// The sweep of the machine cache for session disks that earlier runs
    /// leaked: the directory read, each entry's name read as text only to match
    /// it against the session disks' own naming, and an abandoned disk
    /// deleted.  It raises no card: housekeeping before any guest exists.
    #[cfg(windows)]
    SessionDiskSweep,
    /// A path spelled for this backend's own stderr diagnostics, the
    /// platform's way: an aside to whoever runs the process, which no front-end
    /// words and so no front-end's spelling reaches.  It raises no card: it
    /// only displays a path and opens nothing.
    #[cfg(windows)]
    AsideSpelling,
    /// The lock on the console ring, answering poison by hand as
    /// `lock_ignore_poison` would, since this crate has no ral-core: the ring a
    /// panicked pump left tears nothing, and one torn mid-line still holds the
    /// guest's earlier words.  It raises no card: it guards memory.
    #[cfg(windows)]
    ConsoleRingLock,
    /// The making of the machine cache and of this machine's console log in
    /// it.  It raises no card: a diagnostic file created before any guest
    /// exists, with no shell to route through.
    #[cfg(windows)]
    ConsoleLogOpen,
    /// The sweep of console logs nobody kept from the machine cache: the
    /// directory read, each entry's name read as text only to match it against
    /// this module's own naming, and a stale log deleted.  It raises no card:
    /// housekeeping of this crate's own diagnostics.
    #[cfg(windows)]
    ConsoleLogSweep,
    /// The removal of this machine's own console log once nothing is owed to a
    /// reader.  It raises no card: it deletes a file this crate made.
    #[cfg(windows)]
    ConsoleLogDiscard,
    /// The open of the client end of a machine's own console pipe, asking for
    /// no access, only to wake a pump parked in `ConnectNamedPipe`.  The pipe
    /// is the one this process created and named in the machine's document, and
    /// the handle closes at once with nothing read or written through it.  It
    /// raises no card: no file is opened for its contents.
    #[cfg(windows)]
    ConsolePipeWake,
    /// The check whether the cached wrap of the rootfs is current: the shipped
    /// image's stat, the wrapped copy's presence, and the marker recording
    /// which image was wrapped.  It raises no card: host-side reads of this
    /// crate's own cache before any engine exists.
    #[cfg(windows)]
    RootfsWrapProbe,
    /// The wrapping of the shipped rootfs into the cache: the cache made, the
    /// image copied to a `.part` sibling and renamed into place or removed
    /// after a failure, the wrapped copy opened to append its footer, and the
    /// marker written.  It raises no card: host-side disk plumbing before any
    /// engine exists.
    #[cfg(windows)]
    RootfsWrap,
    /// The open of the machine service's pipe by a client, retried while the
    /// pipe is busy: the client half of the pipe the service listens on, not a
    /// file the model named.  It raises no card: this is host-side, before any
    /// machine exists.
    #[cfg(windows)]
    BrokerPipeConnect,
    /// The walk from the machine service's own executable up to the `target`
    /// directory of the checkout it was built in, and that directory's parent:
    /// where a service registered out of a checkout finds the image pipeline's
    /// output.  The names are the service's own layout, which no grant judges,
    /// so it raises no card.
    #[cfg(windows)]
    CheckoutRoot,
    /// The stat of a boot-media file in the machine service's own installation
    /// or checkout, to choose which layout is there.  The names are the
    /// service's own, which no grant judges, so it raises no card.
    #[cfg(windows)]
    BootMediaProbe,
    /// The open of the folder a client asks to be granted, made under the
    /// client's own impersonated token: that it opens at all is Windows' access
    /// check against the folder's real security descriptor, and the handle,
    /// opened as the folder and never through a link, is the pin held while the
    /// machine serves it.  It raises no card: the machine service has no shell
    /// and no run, and this open is the one check the grant needs.
    #[cfg(windows)]
    ClientFolderOpen,
    /// The path Windows resolves a handle already held on the granted folder
    /// to, with every link on the way resolved.  It raises no card: a name is
    /// read from a handle and nothing is opened.
    #[cfg(windows)]
    HeldFolderPath,
}

impl Site for Silent {}
