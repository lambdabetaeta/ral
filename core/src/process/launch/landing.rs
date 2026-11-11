//! Direct placement and the certificate of a fresh leader in ral's session.

use super::{Launch, Spawned, Unstarted};

use crate::process::group::Step;
use crate::process::{Group, Pgid, Placement, Scope, TerminalHold};

/// A child born leading a fresh group in ral's own session. Birth, not
/// liveness: retain its unreaped child to pin the ID.
pub(crate) struct Leader(Pgid);

impl Leader {
    pub(crate) const fn pgid(&self) -> Pgid {
        self.0
    }
}

impl Launch {
    /// A direct birth: no envelope, so no payload to name or to land on.
    /// `land` receives the pgid the child's own pid makes, its group where
    /// it leads.
    pub(super) fn born_direct<R, G>(
        self,
        step: Step,
        request: R,
        scope: &Scope,
        land: impl FnOnce(R, Pgid) -> Result<(G, TerminalHold), (R, Unstarted)>,
    ) -> Result<Spawned<G>, Unstarted> {
        self.born(step, request, scope, None, |request, child, _| {
            land(request, child)
        })
    }

    /// Spawn under the requested placement, hearing any report before lending.
    pub(crate) fn spawn(
        self,
        placement: Placement,
        scope: &Scope,
    ) -> Result<Spawned<Group>, Unstarted> {
        self.born_direct(placement.step(), placement, scope, Placement::land)
    }

    pub(crate) fn spawn_leader(self, scope: &Scope) -> Result<Spawned<Leader>, Unstarted> {
        self.born_direct(Step::NewLeader, (), scope, |(), child| {
            Ok((Leader(child), TerminalHold::default()))
        })
    }
}

impl Placement {
    #[allow(
        clippy::result_large_err,
        reason = "the request retains captured terminal state until the failed child is reaped"
    )]
    fn land(self, child: Pgid) -> Result<(Group, TerminalHold), (Self, Unstarted)> {
        let group = match self {
            Self::Inherit => Group::Shares,
            Self::NewSession => Group::Owns(child),
            Self::Join(membership) => Group::Joins(membership),
            Self::Tenant(prepared) => {
                let leader = Leader(child);
                return match prepared.lend(&leader) {
                    Ok(terminal) => Ok((Group::Owns(leader.pgid()), terminal)),
                    Err((prepared, error)) => Err((
                        Self::Tenant(prepared),
                        Unstarted::Refused(super::prepare_io(
                            crate::fact::PrepareStep::TerminalHandover,
                            crate::fact::PrepareSubject::Group(
                                u64::from(child.as_raw().unsigned_abs()).into(),
                            ),
                            &error,
                        )),
                    )),
                };
            }
        };
        Ok((group, TerminalHold::default()))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::process::{DurableRoot, Membership, StdioSpec};
    use std::io::{Read, Write};
    use std::sync::mpsc;
    use std::time::Duration;

    const CHILD: &str = "process::launch::landing::tests::landing_child";
    const WAIT: Duration = Duration::from_secs(10);

    #[test]
    fn landing_child() {
        if std::env::var("RAL_LANDING_CHILD").as_deref() != Ok("1") {
            return;
        }
        std::io::stdout().write_all(b"!").unwrap();
        std::io::stdout().flush().unwrap();
        let mut release = [0];
        std::io::stdin().read_exact(&mut release).unwrap();
        assert_eq!(release, [b'G']);
    }

    fn launch() -> (Launch, os_pipe::PipeWriter) {
        let (reader, writer) = crate::process::cloexec_pipe().unwrap();
        let mut launch = Launch::new(std::env::current_exe().unwrap());
        launch
            .args(["--exact", CHILD, "--nocapture", "--test-threads=1"])
            .env("RAL_LANDING_CHILD", "1")
            .stdin(StdioSpec::from_owned(reader))
            .stdout(StdioSpec::Piped)
            .stderr(StdioSpec::Null);
        (launch, writer)
    }

    /// The child stays pinned through all assertions and group operations.
    struct Pending<G> {
        spawned: Spawned<G>,
        owned: Option<Pgid>,
    }

    impl<G> Pending<G> {
        fn ready(&mut self) {
            let mut stdout = self.spawned.child.take_stdout().unwrap();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let mut byte = [0];
                while stdout.read_exact(&mut byte).is_ok() {
                    if byte == *b"!" {
                        tx.send(()).unwrap();
                        return;
                    }
                }
            });
            rx.recv_timeout(WAIT)
                .expect("the landed child must become ready");
        }
    }

    impl<G> Drop for Pending<G> {
        fn drop(&mut self) {
            if let Some(group) = self.owned {
                group.kill();
            }
            let _ = self.spawned.child.kill();
            let _ = self.spawned.child.reap();
            #[cfg(windows)]
            if let Some(group) = self.owned {
                crate::process::release_win_group(group.as_raw());
            }
        }
    }

    #[test]
    fn placement_lands_the_group_its_step_implies() {
        crate::test_helper::subprocess::isolated(
            "process::launch::landing::tests::placement_lands_the_group_its_step_implies",
            || {
                let scope = DurableRoot::default().child();
                let (launch, mut leader_release) = launch();
                let spawned = launch.spawn_leader(&scope).unwrap();
                let owned = Some(spawned.group.pgid());
                let mut leader = Pending { spawned, owned };
                leader.ready();
                let pgid = leader.spawned.group.pgid();
                for placement in [
                    Placement::Inherit,
                    Placement::NewSession,
                    Placement::Join(Membership::new(pgid, Scope::clone(&scope))),
                ] {
                    let leads = placement.step().leads();
                    let (launch, mut release) = self::launch();
                    let spawned = launch.spawn(placement, &scope).unwrap();
                    let owned = spawned.group.owned();
                    let mut child = Pending { spawned, owned };
                    child.ready();
                    let pid = i32::try_from(child.spawned.child.id()).unwrap();
                    assert_eq!(child.spawned.group.owned().is_some(), leads);
                    let expected = match &child.spawned.group {
                        Group::Shares => {
                            #[cfg(unix)]
                            {
                                unsafe { libc::getpgrp() }
                            }
                            #[cfg(windows)]
                            {
                                assert!(!crate::process::group::is_known_group(pid));
                                pid
                            }
                        }
                        Group::Owns(group) => {
                            assert_eq!(group.as_raw(), pid);
                            pid
                        }
                        Group::Joins(membership) => {
                            assert_eq!(membership.group(), pgid);
                            pgid.as_raw()
                        }
                        #[cfg(target_os = "linux")]
                        Group::Enveloped(_) => panic!("a direct launch cannot land enveloped"),
                    };
                    #[cfg(unix)]
                    {
                        assert_eq!(unsafe { libc::getpgid(pid) }, expected);
                        let session = unsafe { libc::getsid(pid) };
                        assert_eq!(
                            session,
                            if leads {
                                pid
                            } else {
                                unsafe { libc::getsid(0) }
                            }
                        );
                    }
                    #[cfg(windows)]
                    if !matches!(child.spawned.group, Group::Shares) {
                        assert!(crate::process::group::is_known_group(expected));
                    }
                    release.write_all(b"G").unwrap();
                }
                leader_release.write_all(b"G").unwrap();
            },
        );
    }

    #[test]
    fn spawn_leader_certifies_its_childs_pid_and_actual_group() {
        crate::test_helper::subprocess::isolated(
            "process::launch::landing::tests::spawn_leader_certifies_its_childs_pid_and_actual_group",
            || {
                let scope = DurableRoot::default().child();
                let (launch, mut release) = launch();
                let spawned = launch.spawn_leader(&scope).unwrap();
                let owned = Some(spawned.group.pgid());
                let mut child = Pending { spawned, owned };
                child.ready();
                let pid = i32::try_from(child.spawned.child.id()).unwrap();
                assert_eq!(child.spawned.group.pgid().as_raw(), pid);
                #[cfg(unix)]
                {
                    assert_eq!(unsafe { libc::getpgid(pid) }, pid);
                    assert_eq!(unsafe { libc::getsid(pid) }, unsafe { libc::getsid(0) });
                }
                #[cfg(windows)]
                assert!(crate::process::group::is_known_group(pid));
                assert_eq!(child.spawned.terminal.pressed(), None);
                release.write_all(b"G").unwrap();
            },
        );
    }
}
