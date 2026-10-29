//! Message payloads, commands and model notices.

use crate::card::elapsed_phrase;
use crate::record::{AgentId, Memory, Settlement};
use ral_core::data::FOValue;
use std::{fmt, time::Duration};

/// Monotonic and never reused, including across `/clear`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ScheduleId(u64);
impl ScheduleId {
    pub(crate) const fn new(id: u64) -> Self {
        Self(id)
    }
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}
impl fmt::Display for ScheduleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read {
    Hatch {
        memory: Memory,
        name: Option<String>,
    },
    Context,
    Resources,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rewrite {
    Clear,
    Evict,
    Rewind(u64),
    Close,
}
impl fmt::Display for Read {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hatch { memory, name } => {
                f.write_str(match memory {
                    Memory::Amnemon => "/new",
                    Memory::Mnemon => "/branch",
                })?;
                name.iter().try_for_each(|name| write!(f, " {name}"))
            }
            Self::Context => f.write_str("/context"),
            Self::Resources => f.write_str("/resources"),
        }
    }
}
impl fmt::Display for Rewrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clear => f.write_str("/clear"),
            Self::Evict => f.write_str("/evict"),
            Self::Rewind(turn) => write!(f, "/rewind {turn}"),
            Self::Close => f.write_str("/close"),
        }
    }
}
/// A working child's life ended with no report.
pub(crate) const ENDED_BEFORE_REPORTING: &str = "ended before reporting";
/// A working child, or a headless root, was cancelled.
pub(crate) const CANCELLED: &str = "cancelled";

#[derive(Clone, Debug)]
pub enum AgentOutcome {
    Replied,
    Stopped(String),
    Cancelled,
    Failed(String),
}
impl AgentOutcome {
    /// Why a child still working ends with this outcome.
    pub(crate) fn reason(self) -> String {
        match self {
            Self::Replied => ENDED_BEFORE_REPORTING.into(),
            Self::Cancelled => CANCELLED.into(),
            Self::Stopped(reason) | Self::Failed(reason) => reason,
        }
    }
}
pub(crate) fn notice(name: &str, settled: &Settlement, elapsed: Duration) -> String {
    let took = elapsed_phrase(elapsed);
    match settled {
        Settlement::Replied => format!(
            "[agent '{name}' replied after {took}: run `` exarch-agents `read '{name}' `` to read it]"
        ),
        Settlement::Failed { reason } => {
            format!("[agent '{name}' failed after {took}: {reason}]")
        }
        Settlement::Stopped => {
            format!("[agent '{name}' stopped after {took}: message it to continue]")
        }
    }
}
#[derive(Clone, Debug)]
pub(crate) struct AgentMessage {
    pub from: AgentId,
    pub from_name: String,
    pub text: String,
    pub from_parent: bool,
}
impl AgentMessage {
    pub(crate) fn render(&self) -> String {
        format!(
            "[EXARCH AGENT {} MESSAGE: {}]\n{}\n[/EXARCH]",
            self.from, self.from_name, self.text
        )
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Source {
    User,
    Schedule,
    Agent,
    Message,
    Read,
    Rewrite,
    Surface,
}
impl Source {
    pub(crate) const ALL: [Self; 7] = [
        Self::User,
        Self::Schedule,
        Self::Agent,
        Self::Message,
        Self::Read,
        Self::Rewrite,
        Self::Surface,
    ];
    pub(crate) fn coalesces(self) -> bool {
        matches!(self, Self::User | Self::Schedule)
    }
}
impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::User => "user",
            Self::Schedule => "schedule",
            Self::Agent => "agent",
            Self::Message => "message",
            Self::Read => "read",
            Self::Rewrite => "rewrite",
            Self::Surface => "surface",
        })
    }
}
pub(crate) fn surface_notice(values: &[FOValue]) -> String {
    values
        .iter()
        .rev()
        .find_map(crate::card::value_to_done)
        .map_or_else(
            || "errand finished.".to_string(),
            |(id, title, elapsed, outcome)| {
                let settled = crate::card::settled_text(Some(id), &title, Some(elapsed), &outcome);
                format!(
                    "{settled}. If you kept its handle and the result is still unclaimed, \
                     `await $h` replays its output and returns its result. `workers` shows \
                     current status; an ID cannot recover a handle."
                )
            },
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parent's own context carries how long the child ran, and a reply
    /// still quotes the fetch command rather than any payload.
    #[test]
    fn a_notice_names_the_fetch_and_the_runtime() {
        let took = Duration::from_secs(72);
        assert_eq!(
            notice("helper", &Settlement::Replied, took),
            "[agent 'helper' replied after 1 min 12 secs: run `` exarch-agents `read 'helper' `` to read it]"
        );
        assert_eq!(
            notice(
                "helper",
                &Settlement::Failed {
                    reason: "boom".into(),
                },
                took,
            ),
            "[agent 'helper' failed after 1 min 12 secs: boom]"
        );
    }

    #[test]
    fn a_completion_notice_names_the_worker_to_await() {
        use ral_core::shell::runtime::{Done, DoneEvent, ErrorRecord, Status, WorkerId};
        let done = DoneEvent {
            id: WorkerId(7),
            title: "building".into(),
            elapsed_ms: 192_000,
            outcome: Done::Err(ErrorRecord::new(
                "<runtime>",
                &Status::Raised(101),
                "build failed",
                None,
            )),
        };
        assert_eq!(
            surface_notice(&[done.to_surface()]),
            "errand #7 (building) finished after 3 mins 12 secs (exit 101): build failed. \
             If you kept its handle and the result is still unclaimed, `await $h` replays \
             its output and returns its result. `workers` shows current status; an ID \
             cannot recover a handle."
        );
    }
}
