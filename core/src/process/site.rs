//! The process launch, wait and jail sites: see `lint/site.rs`.

use crate::site::Site;

/// Sites that raise no card.
#[derive(Clone, Copy)]
pub(in crate::process) enum Silent {
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
}

impl Site for Silent {}

/// Sites that raise a card.
#[cfg(not(windows))]
#[derive(Clone, Copy)]
pub(in crate::process) enum Surface {
    /// The external exec image, built at ral's owned launch boundary.  It
    /// raises an exec card: `finish_command` builds the exec observation,
    /// wrapping the whole dispatch, with the resolved argv and status.
    ProcessLaunch,
}

#[cfg(not(windows))]
impl Site for Surface {}
