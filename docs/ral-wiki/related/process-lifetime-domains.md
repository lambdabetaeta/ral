---
verified_at_commit: 64b60eaf
verified_at_date: 2026-11-10
against: [design/exec, design/residency, internals/pipeline-execution, internals/linux-envelope]
---

# Process lifetime domains

**A process group is a signal-delivery set, not an ownership boundary.** A
program may create sessions and groups without intending to outlive its caller.
Three independent obligations underlie [[design/exec|exec]]:

- **Identity:** an operation still addresses the original kernel object.
- **Containment:** descendants cannot leave the owner's termination domain by
  reorganising their processes.
- **Completion:** the owner knows its domain contains no live process. Sending a
  kill, observing the initial process exit, and receiving output EOF prove
  different things; none alone proves this one.

A fourth obligation is **owner-failure cleanup**: something must still act if
ral itself dies. A destructor cannot provide this after `SIGKILL`.

## Linux

**Cgroup v2 separates lifetime membership from sessions and process groups.**
The kernel's [cgroup-v2 specification][cgroups] establishes:

- A fork inherits cgroup membership. `setsid` and `setpgid` do not change it.
- Writing `1` to a non-root domain cgroup's `cgroup.kill` kills its subtree,
  accounting for concurrent forks and migrations. This is not a general
  `SIGTERM` interface.
- `cgroup.events` reports `populated=0` when the subtree has no live process.
  This does not prove zombies have been reaped. Membership must be closed to
  outside admission before emptiness becomes a lasting completion fact.
- Delegation containment depends on access to the destination and common
  ancestor's `cgroup.procs`, or on namespace delegation. A directory owned by
  the same uid as an unrestricted payload does not by itself separate their
  authority. A cgroup namespace is not automatically an access restriction.

[systemd delegation][delegation] supplies an exclusive subtree to a service or
scope with `Delegate=`; it is not permission to modify systemd's other cgroups.
Controller delegation for resource budgets is a separate requirement from
membership and killing. Neither cgroup creation nor `cgroup.kill` arms cleanup
on the owner's death: a surviving manager must supply that policy.

A [PID namespace][pidns] supplies another containment primitive: the death of
its init causes the kernel to kill all processes in the namespace, including
those which changed sessions. It changes PID visibility, needs permission to
create the namespace, and does not let an internal process detach beyond that
namespace. It is not a transparent substitute for native host execution.
A [subreaper][subreaper] adopts orphans; adoption is not containment.

A group-directed [pidfd][pidfd] addresses an original group identity without
converting it back to a recyclable number. It does not contain group members.
The descriptor must be acquired while identity is known, not from a stale PID
reported earlier. Linux 6.9 introduced the group-signalling flag; the
[kernel implementation][pidfd-source] uses the held `struct pid` as the group
identity.

## Windows

**A Job Object can contain descendant processes independently of console
process groups.** Microsoft's [Job Objects][jobs] and [Nested Jobs][nested]
contracts distinguish:

- Normal process creation inherits job membership. An outer job that permits
  neither breakaway mode retains descendants, including members of nested jobs.
  Valid nesting must be checked; not every combination of job restrictions
  admits it.
- `TerminateJobObject` terminates a job and its child jobs.
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` acts when the **last** job handle closes;
  handles retained by other processes change that lifetime.
- Completion-port delivery is [not generally guaranteed][completion], including
  `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO`. Notifications can wake a reader, but
  [accounting state][accounting] must establish completion. Admission must first
  close, as on Linux; a job handle is not a general waitable empty-event.
- A request to an external service is not inherited process creation. Microsoft
  explicitly excludes `Win32_Process.Create` from automatic job association.

## macOS

**No documented ordinary-application equivalent was established by this
investigation.** This is a limit of the examined interfaces, not a claim that
no privileged or future mechanism can exist.

- `launchd`'s [documented cleanup][launchd] kills processes with the job's PGID.
  `AbandonProcessGroup=false` does not capture descendants which changed groups.
- XNU's [event header][event-header] says `NOTE_TRACK`, `NOTE_TRACKERR` and
  `NOTE_CHILD` have been unsupported since 10.5; its
  [process filter][event-source] rejects them with `ENOTSUP`. Fork notifications
  and descendant enumeration are not a closed membership set.
- XNU's [coalition syscall][coalition] requires membership in a privileged
  coalition, not merely uid 0. Its termination request arms an empty notification
  and prevents explicit new admissions; existing members may still fork. It is
  not a kill-all operation.
- XNU has a closer admission primitive in [personas][persona]: fork inherits
  the parent's persona, and `kpersona_dealloc_syscall` explicitly invalidates it
  to deny subsequent spawn/fork. Allocation and invalidation require
  `com.apple.private.persona-mgmt`; root alone does not satisfy those checks.
  This is a candidate ingredient, not a proved backend: access on shipping
  macOS, in-flight births, membership observation, termination and completion
  still need evidence.
- [Audit-token signalling][audit-signal] validates process identity in the kernel
  before signalling the referenced process. It avoids the user-space
  identity-check-then-`kill(pid)` race, subject to ordinary signal permission and
  MAC policy. These private libproc interfaces address one process; they do not
  supply descendant membership or close admission.
- Apple's newer [descendants Endpoint Security client][es-descendants], marked
  macOS 27.0, observes an entire descendant subtree without root or TCC approval,
  but still requires the restricted Endpoint Security entitlement. Its documented
  contract is event observation/authorization, not atomic subtree termination
  or kill-on-owner-death. It also restricts non-root setuid/setgid execution.
  Endpoint Security documents [dropped events][es-sequence]; a consumer-built
  PID registry is not thereby a proven containment mechanism.

A Linux VM can supply Linux's containment for Linux programs. It cannot make
native macOS executables run unchanged. Requiring a VM is an execution-platform
choice, not an invisible implementation detail.

## Rust implementations examined

**Crates provide useful cleanup mechanisms, but their achieved guarantees differ.**
Published source inspected on 2026-11-10, without executing the crates:

- **[process-wrap 10.0.1][process-wrap]:** its Unix `ProcessGroupChild` signals
  with `killpg`. Session/group changes still escape that delivery; waiting for
  children with `waitpid` does not establish that all historical descendants died.
- **[cosca 0.6.0][cosca]:** macOS `FdMarker` identifies an inherited pipe across
  group changes and reparenting. Its source explicitly documents escape by
  descriptor closing/scrubbing and missed orphan members after credential
  changes. Closing all markers is not proof that all their former holders exited.
- **[procage 0.7.1][procage]:** macOS embeds a per-run marker in the inherited
  Seatbelt policy, detected by contrasting two `mach-lookup` permission queries.
  Unlike an fd marker, ordinary descriptor cleanup does not erase it. This is
  a concrete route to finding daemonised descendants without persona-management
  entitlement. However, `lifetime::{members,reap,end}` enumerates and kills by
  numeric PID, for up to 64 rounds; `Child::kill_all` documents the PID-reuse
  race. The loop has no closed-admission barrier, and the source returns success
  on an empty scan. That is not a kernel-certified empty domain or owner-death
  guarantee. Visibility, credentials and policy composition also need validation.

Procage's inherited-policy identity is worth investigating before requiring
Apple-private persona management. Importing the crate unchanged would not resolve
ral's original identity-safety obligation; atomic audit-token signalling addresses
one part, not the completeness of discovery or closure of membership.

## What ral currently provides

**The implementations do not yet share a descendant-containment contract.**
At the verified revision:

- Individual Unix children signal through `Watch`, which retains their
  unreaped identity. Group signalling instead lives on copyable `Pgid`; its
  lifetime witness is elsewhere, in a leader pin or pipeline anchor.
- An envelope's payload group has no such pin. Holding bwrap's monitor does not
  hold its different leader ([[internals/linux-envelope|Linux envelope]]).
- Guest commands acquire cgroups before exec and drop to fresh uids. Their
  `JailCgroup` destructor requests a kill and retries directory removal briefly;
  neither operation returns a completion proof. The guest init also cleans a
  dead engine's cgroups (`ral-guest/src/init/halt.rs`).
- Windows assigns suspended children to Jobs before resuming, but
  `install_kill_on_close` ignores its setup result and `wait_job` relies on the
  completion message. These are gaps against the stronger contract above.
- Native Unix groups do not retain a descendant which leaves them.

The relevant code is `core/src/process/{group.rs,reaper/unix.rs,jail.rs}`,
`process/group/windows.rs`, `process/jail/linux.rs`, and
`shell/exec/pipeline/{collect,stage}.rs`. See
[[internals/pipeline-execution|pipeline execution]] and
[[design/residency|residency]] for the existing ownership and settlement rules.

## Boundary of the claim

**Containing descendants does not contain every effect they can request.**
A service manager, GUI launcher, remote server, or other already-running deputy
may create work outside the domain. Preventing that requires authority policy
for the deputy, not another process-tree traversal. Likewise, a privileged
payload able to change the containment mechanism's policy is outside that
mechanism's proof unless privilege is separately restricted.

A stable signalling handle and a contained lifetime domain solve different
problems. Neither should be inferred from a numeric PID or PGID, an output pipe,
or the continued existence of a wrapper process.

[process-wrap]: https://docs.rs/crate/process-wrap/10.0.1/source/src/std/process_group.rs
[cosca]: https://docs.rs/crate/cosca/0.6.0/source/src/containment/fdmarker.rs
[procage]: https://docs.rs/crate/procage/0.7.1/source/src/macos/lifetime.rs
[cgroups]: https://github.com/torvalds/linux/blob/v6.12/Documentation/admin-guide/cgroup-v2.rst
[delegation]: https://systemd.io/CGROUP_DELEGATION/
[pidns]: https://man7.org/linux/man-pages/man7/pid_namespaces.7.html
[subreaper]: https://man7.org/linux/man-pages/man2/PR_SET_CHILD_SUBREAPER.2const.html
[pidfd]: https://man7.org/linux/man-pages/man2/pidfd_send_signal.2.html
[pidfd-source]: https://github.com/torvalds/linux/blob/v6.12/kernel/signal.c
[jobs]: https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects
[nested]: https://learn.microsoft.com/en-us/windows/win32/procthread/nested-jobs
[completion]: https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_associate_completion_port
[accounting]: https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_basic_accounting_information
[launchd]: https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5
[event-header]: https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/sys/event.h
[event-source]: https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_event.c
[coalition]: https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/sys_coalition.c
[persona]: https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/sys_persona.c
[audit-signal]: https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/proc_info.c
[es-descendants]: https://developer.apple.com/documentation/endpointsecurity/es_new_descendants_client(_:_:)
[es-sequence]: https://developer.apple.com/documentation/endpointsecurity/es_message_t/seq_num
