//! The envelope's processes as the host numbers them.  A kernel deny line
//! carries the host pid of the process it denied, which no process inside
//! the pid namespace can name, so the chain is read from the host's `/proc`
//! while it runs.

use crate::process::Pgid;
use crate::site::{Silent, site};
use std::collections::HashSet;

/// The host pids at and beneath the payload group's `leader` that live now:
/// the pid namespace's init, the supervisor and its program (with no
/// namespace, the leader is the supervisor), and whatever each has forked.
/// A process already gone adds nothing.
pub(crate) fn pids(leader: Pgid) -> HashSet<u32> {
    let leader = leader.as_raw().cast_unsigned();
    let mut seen = HashSet::from([leader]);
    let mut frontier = vec![leader];
    while let Some(pid) = frontier.pop() {
        for child in children(pid) {
            if seen.insert(child) {
                frontier.push(child);
            }
        }
    }
    seen
}

/// `pid`'s children, from each of its threads' own list.
fn children(pid: u32) -> Vec<u32> {
    let Ok(tasks) = site!(
        Silent::EnvelopeChain,
        std::fs::read_dir(format!("/proc/{pid}/task"))
    ) else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter_map(|task| {
            site!(
                Silent::EnvelopeChain,
                std::fs::read_to_string(task.path().join("children"))
            )
            .ok()
        })
        .flat_map(|list| {
            list.split_whitespace()
                .filter_map(|child| child.parse().ok())
                .collect::<Vec<u32>>()
        })
        .collect()
}
