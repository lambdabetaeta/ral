// The typed-site vocabulary, shared by every crate: see `lint/site.rs`.
include!("../../lint/site.rs");

/// Sites that raise no card.
#[derive(Clone, Copy)]
pub(crate) enum Silent {
    // The host process's environment, working directory and identity.
    /// A variable of the host process environment, as text: the one reader
    /// that [`crate::host::var`] publishes, which the host-only facts of
    /// `host` (`HOME`, `USER`, the XDG bases) and every other host-level read
    /// go through.  A `within [env: …]` overlay never applies there.  It
    /// raises no card: reading a variable opens no object.
    HostVar,
    /// A variable of the host process environment as the OS holds it, for a
    /// value that is a path or that need not be Unicode
    /// ([`crate::host::var_os`]): the same reader as `HostVar`, undecoded.  It
    /// raises no card: reading a variable opens no object.
    HostVarOs,
    /// The host process environment, enumerated as the OS holds it
    /// ([`crate::host::vars_os`]): the one enumeration of the host, for the
    /// `env` builtin, a child's inherited block and the like.  It raises no
    /// card: it reads variables, not objects.
    HostVars,
    /// The host's temp directory ([`crate::host::temp_dir`]): `TMPDIR`,
    /// `TMP` or `TEMP`, else the OS's default.  It raises no card: it reads the
    /// host's environment and opens no object.
    HostTemp,
    /// The account's home from the user database, `std::env::home_dir`'s rule
    /// (`getpwuid_r` on Unix, the profile folder on Windows), asked only where
    /// no `HOME` is bound: the fallback inside `host::xdg` and `host::home_dot`,
    /// so ral's own layout resolves under `env -i`, cron and systemd units.  A
    /// `~` in a script never reads it.  It raises no card: it reads the user
    /// database, not an object a model named.
    AccountHome,
    /// The host process's working directory, for a caller with no shell to
    /// ask.  It raises no card: it reads one fact of the host, not an object
    /// a model names.
    HostCwd,
    /// The host's `HOME` and `USER`, read to seed the baseline `env` of a
    /// fresh shell at boot: the host environment is the source the overlay
    /// later shadows.  It raises no card: it reads variables, not objects.
    HostEnvSeed,
    /// The process's home, which the fs floor lists among its defaults: the
    /// floor has no `FreezeCtx`, so its home is the process's, as the places
    /// `home` defaults to are.  It raises no card: it reads a variable, not
    /// an object.
    FloorHome,

    // Process spawn, wait and the jail.
    /// The one pipe constructor: `os_pipe::pipe` under the spawn lock's shared
    /// side, so a fork on another thread never inherits the window in which
    /// Apple has yet to set `CLOEXEC`.  It raises no card: an anonymous pipe
    /// names no object.
    CloexecPipe,
    /// The one socketpair constructor: `UnixStream::pair` under the spawn
    /// lock's shared side, closing the same window as `CloexecPipe`.  It
    /// raises no card: an anonymous socketpair names no object.
    #[cfg(unix)]
    CloexecSocketpair,
    /// The one fork of a child: `Command::spawn` under the spawn lock's
    /// exclusive side.  Every other production fork goes through this or the
    /// `output` and `status` helpers beside it.  It raises no card: this is
    /// the lock, not the exec, and what is run is surfaced where its caller
    /// built it.
    ForkSpawn,
    /// `Command::status`'s own wait, taken outside the spawn lock on a child
    /// nobody parks, so a stop is not a case here.  It raises no card: it
    /// observes a child its caller already launched.
    StatusWait,
    /// The one raw `Child::wait` of `ChildHandle::reap`: the blocking reap
    /// after a confirmed `SIGKILL`, which terminates even a stopped process.
    /// Every other wait is the reaper's.  It raises no card: it observes no
    /// object a model named.
    ChildReap,
    /// `/bin/ps`, spawned to sample the live process tree so a sandbox denial
    /// can be attributed to a descendant of ral: denial records carry only a
    /// command name and a pid.  It raises no card: a diagnostic probe, not
    /// turn-time model data I/O.
    PsSample,
    /// `journalctl -k`, or `dmesg` where the journal is absent or unreadable,
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
    /// The guest-global jail sequence counter file, opened and locked to mint
    /// the next uid and cgroup number.  It raises no card: guest-boot and
    /// exec-time jail setup, not model turn-time I/O.
    #[cfg(target_os = "linux")]
    JailSeqFile,
    /// A cgroup control file of the jail (`memory.max`, `pids.max`,
    /// `cpu.max`, `cgroup.subtree_control`), written to set its limits.  It
    /// raises no card: the limits are policy the host sets, never data the
    /// model reads or writes.
    #[cfg(target_os = "linux")]
    JailCgroupWrite,
    /// Whether the jail's own cgroup directory exists, asked before the chain
    /// of its ancestors is walked to delegate controllers.  It raises no
    /// card: the jail, not a name any grant judges.
    #[cfg(target_os = "linux")]
    JailCgroupProbe,
    /// `cgroup.procs` of a jailed exec's cgroup, opened to be kept across the
    /// fork so the pre-exec closure can place the child without a second open
    /// after the uid drop.  It raises no card: guest-boot and exec-time jail
    /// setup, not model I/O.
    #[cfg(target_os = "linux")]
    JailCgroupProcs,
    /// A directory of the jail's tree made where absent: the cgroup of a
    /// jailed exec, the ancestors above it, and the directory of the sequence
    /// counter.  The kernel makes a cgroup of a directory, and the names are
    /// the daemon-set jail root's, so it raises no card: guest-boot and
    /// exec-time jail setup, not model I/O.
    #[cfg(target_os = "linux")]
    JailCgroupMake,
    /// A jailed exec's cgroup directory, removed once its members are dead.
    /// The kernel refuses it while any live, and the name is the jail's own,
    /// so it raises no card: exec-time jail teardown, not model I/O.
    #[cfg(target_os = "linux")]
    JailCgroupRemove,

    // The sandbox: pins, respawn and rendering.
    /// A launch of `/bin/true` under a `Restricted` envelope, spawned to ask
    /// whether this host builds one at all.  A host capability probe for
    /// tests, not a grant spawn, so it raises no card.
    #[cfg(all(target_os = "linux", any(test, feature = "test-util")))]
    RestrictedEnvelopeProbe,
    /// ral's own pin, run as an anchor under bwrap to ask whether this host
    /// builds an envelope at all.  A setup-time host capability probe, not a
    /// model exec image, so it raises no card.
    #[cfg(target_os = "linux")]
    BwrapHostProbe,
    /// Ral's own `/proc/self/cgroup`, read to re-root the envelope's cgroup
    /// tree on the namespace's root.  It raises no card: envelope
    /// construction, not a model's data I/O.
    #[cfg(target_os = "linux")]
    OwnCgroup,
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
    /// The head of a seed, a hop or a refused exec's target, read by name to
    /// learn its interpreter as the kernel will.  It raises no card: sandbox
    /// rendering, not the model's data I/O.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    CarrierShebang,

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

    // Locks.
    /// The ignore-poison policy, applied once here: every other lock goes
    /// through these helpers.  It raises no card: it guards in-process state,
    /// not a named object.
    IgnorePoison,
    /// The strict-poison policy, applied once here
    /// ([`crate::sync::LockExt::lock_strict`]): a lock whose guarded state
    /// restates something outside the process stays poisoned once a holder
    /// panics, since a recovered guard would resume over a torn restatement.
    /// It raises no card: it guards in-process state, not a named object.
    KeepPoison,

    // The Windows DACL ledger and stamp store.
    /// The target of the link that redirected a frozen allowance, read to
    /// name it in the refusal.  It raises no card: sandbox setup, not model
    /// data I/O.
    #[cfg(windows)]
    DaclFrozenLink,
}

impl Site for Silent {}

/// Sites that raise a card.
#[cfg(not(windows))]
#[derive(Clone, Copy)]
pub(crate) enum Surface {
    /// The external exec image, built at ral's owned launch boundary.  It
    /// raises an exec card: `finish_command` builds the exec observation,
    /// wrapping the whole dispatch, with the resolved argv and status.
    ProcessLaunch,
}

#[cfg(not(windows))]
impl Site for Surface {}
