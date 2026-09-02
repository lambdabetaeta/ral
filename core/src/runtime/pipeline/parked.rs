//! A pipeline frozen by Ctrl-Z: the process group, the Ctrl-Z gate, and the
//! collector's still-unobserved stages, held until `fg`, `bg`, the sweep, or
//! the REPL's exit `cleanup` resumes, polls, or tears it down.
//!
//! Unix only: there is no stop to park from on Windows.

use super::collect::{CollectState, Drive, Pass};
use super::group::PipelineGroup;
use crate::ir::PipeYield;
use crate::process::{CancelCause, Pgid, Signal, StageGate};
use crate::types::{Mooring, Shell};
use std::sync::Arc;

/// What `join` deposits into [`crate::types::SessionState::parked`] for the
/// job table to drive.  Fields `pub(super)`: `PipeNode::join` is the sole
/// constructor.
pub struct ParkedPipeline {
    pub(super) group: PipelineGroup,
    pub(super) gate: Arc<StageGate>,
    pub(super) collect: CollectState,
    pub(super) yields: PipeYield,
    pub(super) cmd: String,
}

/// `resume_and_collect`'s outcome: the pipeline ran to completion, or parked
/// again at a later stop.
///
/// `Parked` boxes the pipeline: `ParkedPipeline` dwarfs `Finished`, and
/// clippy's `large_enum_variant` wants the smaller variant off the stack.
pub enum Resumed {
    Finished { completed: bool },
    Parked(Box<ParkedPipeline>, Signal),
}

/// `poll`'s outcome: the job table sweep's non-blocking probe.
pub enum ParkedPoll {
    Running,
    Finished { completed: bool },
    Stopped(Signal),
}

impl ParkedPipeline {
    pub fn pgid(&self) -> Pgid {
        self.group.leader_pgid()
    }

    pub fn cmd(&self) -> &str {
        &self.cmd
    }

    /// `gate.resume(); collect.resume_all(); SIGCONT -pgid`.  Touches no
    /// terminal state — `fg`'s own guard handles that, through the door
    /// `wait_foreground` already opens.
    pub fn resume(&mut self) {
        self.gate.resume();
        self.collect.resume_all();
        self.pgid().signal_group(Signal::new(libc::SIGCONT));
    }

    /// `resume()`, then drive to `Done` (fold, drop `self`) or the next stop.
    pub fn resume_and_collect(mut self, mooring: &Mooring, shell: &mut Shell) -> Resumed {
        self.resume();
        let Self {
            mut group,
            gate,
            mut collect,
            yields,
            cmd,
        } = self;
        match collect.drive(&mut group, &gate, mooring, shell) {
            Drive::Done => {
                let completed = collect.fold(mooring, shell).finish(yields).is_ok();
                Resumed::Finished { completed }
            }
            Drive::Parked(sig) => {
                group.release_foreground_and_relay();
                Resumed::Parked(
                    Box::new(Self {
                        group,
                        gate,
                        collect,
                        yields,
                        cmd,
                    }),
                    sig,
                )
            }
        }
    }

    /// One non-blocking drive pass without `SIGCONT`: for the job table's
    /// sweep of a backgrounded job.
    pub fn poll(&mut self, mooring: &Mooring, shell: &mut Shell) -> ParkedPoll {
        match self.collect.pass(&mut self.group, &self.gate, mooring, shell) {
            Pass::Done => {
                let completed = self
                    .collect
                    .fold(mooring, shell)
                    .finish(self.yields)
                    .is_ok();
                ParkedPoll::Finished { completed }
            }
            Pass::Parked(sig) => ParkedPoll::Stopped(sig),
            Pass::Advanced | Pass::Idle => ParkedPoll::Running,
        }
    }

    /// Signal the group, cancel and wake every stage, and open the gate so
    /// cancelled threads can leave.  The caller's drop then joins them.
    pub fn cancel(&mut self, cause: CancelCause) {
        self.group.signal(cause);
        self.collect.cancel_stages(cause);
        self.gate.resume();
    }
}
