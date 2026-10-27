// The typed-site vocabulary, shared by every crate: see `lint/site.rs`.
include!("../../lint/site.rs");

/// Sites that raise no card.
#[derive(Clone, Copy)]
pub(crate) enum Silent {
    /// Canonical self path for a role exec'd by /bin/sh; startup infrastructure.
    #[cfg(unix)]
    IndirectReexec,
    /// The read of git discovery's `.git` pointer file and the reciprocal
    /// `gitdir` file it names (`freeze::ungoverned`), made at session start to
    /// freeze policy, before any grant exists to govern it.  It raises no card:
    /// it reads the user's working tree to decide the grant, not on a model's
    /// request.
    GitDiscoveryRead,
    /// The enumeration of the host's dynamic linkers: the files matching the
    /// `ld*.so*` globs under the system library directories, walked so that
    /// each can be admitted and bound read-only beside the grant, since
    /// `execve` of a dynamic binary needs its `PT_INTERP` file.  It raises no
    /// card: it is sandbox support naming the host's own files, not a read a
    /// model asked for.
    #[cfg(target_os = "linux")]
    LinkerRegion,
    // Bootstrap callers outside host own these sites; host/site.rs owns the readers.
    /// The host's `HOME` and `USER`, read to seed the baseline `env` of a
    /// fresh shell at boot: the host environment is the source the overlay
    /// later shadows.  It raises no card: it reads variables, not objects.
    HostEnvSeed,
    /// The process's home, used to exclude user paths from system reads.
    /// It raises no card: it reads a variable, not an object.
    SystemReadsHome,

    // Sandbox diagnostics.
    /// Fixed system paths for `journalctl -k` and `dmesg`, probed and
    /// spawned to harvest the kernel's seccomp-denial audit window for a
    /// sandbox diagnostic.  It raises no exec card: a post-mortem diagnostic
    /// probe, not the model's exec image.
    #[cfg(target_os = "linux")]
    JournalRead,
    /// `/usr/bin/log show`, spawned to read Seatbelt denial records from the
    /// unified log for a sandbox diagnostic.  It raises no card: a
    /// diagnostic probe, not turn-time model data I/O.
    #[cfg(target_os = "macos")]
    LogShow,

    // The sandbox: pins, respawn and rendering.
    /// Query the kernel's Landlock ABI; confinement setup raises no card.
    #[cfg(target_os = "linux")]
    LandlockVersion,
    /// Create the launch's Landlock ruleset; confinement setup raises no card.
    #[cfg(target_os = "linux")]
    LandlockCreate,
    /// Admit a held object to Landlock; confinement setup raises no card.
    #[cfg(target_os = "linux")]
    LandlockAdmit,
    /// Enter the payload's Landlock domain; confinement setup raises no card.
    #[cfg(target_os = "linux")]
    LandlockRestrict,
    /// Sweep inherited descriptors before the payload runs; no model I/O.
    #[cfg(target_os = "linux")]
    InheritedSweep,
    /// Detach's second fork surrenders process ownership; its exec card already fired.
    #[cfg(unix)]
    DetachFork,
    /// A reaper test forks an exit-status fixture; no model I/O.
    #[cfg(all(unix, test))]
    ReaperExitFork,
    /// ral's own pin, run as an anchor under bwrap to ask whether this host
    /// builds an envelope at all.  A setup-time host capability probe, not a
    /// model exec image, so it raises no card.
    #[cfg(target_os = "linux")]
    BwrapHostProbe,
    /// Host pty names, masked during envelope construction.
    #[cfg(target_os = "linux")]
    PtyDiscovery,
    /// Ral's own `/proc/self/cgroup`, read to re-root the envelope's cgroup
    /// tree on the namespace's root.  It raises no card: envelope
    /// construction, not a model's data I/O.
    #[cfg(target_os = "linux")]
    OwnCgroup,
    /// `/dev` and `/dev/dri`, listed at each launch for the GPU's device nodes,
    /// bwrap having no globs.  It raises no card: envelope construction, not a
    /// model's data I/O.
    #[cfg(target_os = "linux")]
    GpuDiscovery,
    /// The `Command` for a boot-pinned sandbox binary (the ral re-exec for
    /// helpers and bundled tools, the bwrap envelope), built over the pin's
    /// exec path.  It raises no card: an infrastructure spawn, not a model
    /// exec image, which surfaces at `command::run`.
    #[cfg(unix)]
    PinnedExec,
    /// The `/proc/self/fd/<N>` magic link of a boot-pinned sandbox binary,
    /// read to find the inode's current path for the envelope's own read-only
    /// bind.  It raises no card: sandbox exe-pinning infrastructure, not the
    /// model's data I/O.
    #[cfg(target_os = "linux")]
    PinLocate,
    /// The `Command` of the ral re-exec for helper subprocesses (the pipeline
    /// anchor, the bundled-tool multicall), built over the live `current_exe`
    /// when no boot pin exists.  It raises no card: an infrastructure spawn,
    /// not a model exec image, which surfaces at `command::run`.
    #[cfg(unix)]
    SelfReexec,
    /// A boot-time sandbox binary (ral itself, the bwrap envelope), opened to
    /// pin it by descriptor, immune to on-disk swaps.  It raises no card:
    /// sandbox exe-pinning infrastructure, not the model's data I/O.
    #[cfg(target_os = "linux")]
    PinOpen,
    /// The `Command` of the sandbox respawn handoff, built for the confined
    /// re-exec.  It raises no card: the surface card fired before the
    /// handoff, so the exec itself raises none.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    RespawnSpawn,
    /// The `exec` of the sandbox respawn handoff, which replaces this process
    /// image with the confined target.  It raises no card: the surface card
    /// fired before the handoff, so the exec itself raises none.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    RespawnExec,
    /// A stat and an access check by name under the directory handle a walk
    /// holds, asking whether the entry is still the walked object and beyond
    /// the user's write.  It raises no card: sandbox rendering over the walk's
    /// own names, not a model's data I/O.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    CarrierSystemOnly,
    /// Read through a system-only walk's handle; only the system can author it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    CarrierRead,

    // The wire and the engine process.
    /// The engine's exit when adopting the wire on fd 3 fails.  The process
    /// has adopted fd 3 and nothing else: no shell is booted, so there is no
    /// lease, no watched child and no staged write to unwind.  It raises no
    /// card: nothing a model named is touched.
    #[cfg(unix)]
    EngineAdoptExit,
    /// The engine's exit once `engine_session` returns.  Its teardown settle
    /// is the shutdown: it cancels the durable root and waits the worker out
    /// before returning.  An engine's stdio is `/dev/null`, so its session
    /// mints no `TerminalLease` and no `TerminalLoan` can exist to strand.  It
    /// raises no card: nothing a model named is touched.
    #[cfg(unix)]
    EngineSessionExit,
    /// The engine process's working directory, set when an `Attach` restores
    /// the root's cwd: it is the process state of a carrier that owns its
    /// process, not the shell's logical cwd.  It raises no card: it opens no
    /// object a model names.
    #[cfg(unix)]
    EngineProcessCwd,
    /// A loopback connection, made and accepted for a front-end and an engine
    /// that share a process tree: the Windows twin of `CloexecSocketpair`,
    /// spelled as a bind, connect and accept because Windows has no
    /// `socketpair(2)`.  No outside name is reached (the port is ephemeral and
    /// the peer is this process), so it raises no card, for the same reason
    /// its Unix twin raises none.
    #[cfg(all(windows, test))]
    WirePairWindows,

    // Command heads, the terminal device, bootstrap and bundled tools.
    /// Metadata questions of an admitted program's real path, asked by name
    /// after its exec failed: is it a directory, does it exist, is it
    /// executable.  They grant nothing, so they raise no card.
    HeadRefusal,
    /// Which extension names a sibling of an admitted program's real path,
    /// asked by name after its exec failed.  It grants nothing, so it raises
    /// no card.
    HeadSuffixed,
    /// The `ask` builtin's controlling-terminal device (`/dev/tty`, or
    /// `CONIN$` and `CONOUT$` on Windows), opened to prompt and read one line
    /// directly from the user, bypassing redirection.  It raises no card: a
    /// terminal-device interaction, not turn-time model data I/O.
    AskTty,
    /// The kernel's copy of this process's own argv (`/proc/self/cmdline`),
    /// read before `main` on a non-glibc Linux, where std has captured none.
    /// It raises no card: process bootstrap, not model data I/O.
    #[cfg(ctor_without_argv)]
    ArgvBeforeMain,
    /// The byte read of a file the bundled `diff` tool compares.  The bundled
    /// exec is surfaced once at the exec site (`finish_command`), and this
    /// internal shuffling rides that visible call, so it raises no separate
    /// read card.
    #[cfg(feature = "diffutils")]
    DiffRead,
    /// The exit of a process that has served a re-exec role
    /// ([`crate::invocation::exit_if_served`]): the anchor drains a pipe, and
    /// the sandbox stage has already exec'd or refused.  Nothing is booted in
    /// such a process, so no lease, watched child or staged write is left
    /// behind, and in a `#[ctor]` nothing at all exists yet.  It raises no
    /// card: no shell exists to raise one.
    ServedRoleExit,

    // The Windows DACL ledger and stamp store.
    /// The target of the link that redirected a frozen allowance, read to
    /// name it in the refusal.  It raises no card: sandbox setup, not model
    /// data I/O.
    #[cfg(windows)]
    DaclFrozenLink,
}

impl Site for Silent {}
