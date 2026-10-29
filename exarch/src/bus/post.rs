//! Deliveries and commands yielded by the inbox.

use crate::card::elapsed_phrase;
use crate::record::{AgentId, Memory, Settlement, Voice};
use ral_core::data::FOValue;
use std::{fmt, sync::Arc, time::Duration};

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
#[derive(Clone, Debug)]
pub enum AgentOutcome {
    Replied,
    Stopped(String),
    Cancelled,
    Failed(String),
}
impl AgentOutcome {
    /// Why a child still working ends with this outcome.
    pub(crate) fn reason(&self) -> String {
        match self {
            Self::Replied | Self::Cancelled => "cancelled".into(),
            Self::Stopped(reason) | Self::Failed(reason) => reason.clone(),
        }
    }
}
fn notice(name: &str, settled: &Settlement, elapsed: Duration) -> String {
    let took = elapsed_phrase(elapsed);
    match settled {
        Settlement::Replied => format!(
            "[agent '{name}' replied after {took}: run `` exarch-agents `read '{name}' `` to read it]"
        ),
        Settlement::Failed { reason } => {
            format!("[agent '{name}' failed after {took}: {reason}]")
        }
    }
}
#[derive(Clone, Debug)]
pub(crate) struct AgentMessage {
    pub from: AgentId,
    pub from_name: String,
    pub text: String,
}
impl AgentMessage {
    pub(super) fn render(&self) -> String {
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
    Nudge,
    Read,
    Rewrite,
    Surface,
}
impl Source {
    pub(crate) const ALL: [Self; 8] = [
        Self::User,
        Self::Schedule,
        Self::Agent,
        Self::Message,
        Self::Nudge,
        Self::Read,
        Self::Rewrite,
        Self::Surface,
    ];
    pub(crate) fn coalesces(self) -> bool {
        matches!(self, Self::User | Self::Schedule | Self::Nudge)
    }
}
impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::User => "user",
            Self::Schedule => "schedule",
            Self::Agent => "agent",
            Self::Message => "message",
            Self::Nudge => "nudge",
            Self::Read => "read",
            Self::Rewrite => "rewrite",
            Self::Surface => "surface",
        })
    }
}
fn surface_notice(values: &[FOValue]) -> String {
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
#[derive(Clone, Debug)]
pub(crate) enum Next {
    Item(Item),
    Read(Read),
    Rewrite(Rewrite),
}
#[derive(Clone, Debug)]
pub(crate) enum Item {
    Human(String),
    Wakeup(String),
    Agent {
        name: Arc<str>,
        settled: Settlement,
        elapsed: Duration,
    },
    Message(AgentMessage),
    Nudge {
        prompt: u64,
        text: String,
    },
    Surface {
        id: AgentId,
        values: Vec<FOValue>,
    },
}
impl Item {
    pub(crate) fn text(&self) -> String {
        match self {
            Self::Human(s) | Self::Wakeup(s) => s.clone(),
            Self::Nudge { text, .. } => text.clone(),
            Self::Agent {
                name,
                settled,
                elapsed,
            } => notice(name, settled, *elapsed),
            Self::Message(m) => m.render(),
            Self::Surface { values, .. } => surface_notice(values),
        }
    }
    pub(crate) fn voice(&self) -> Voice {
        match self {
            Self::Human(_) => Voice::Human,
            Self::Wakeup(_) => Voice::Wakeup,
            Self::Message(_) => Voice::Peer,
            Self::Agent { .. } | Self::Nudge { .. } | Self::Surface { .. } => Voice::Harness,
        }
    }
    pub(crate) fn opens_exchange(&self) -> bool {
        !matches!(self, Self::Nudge { .. })
    }
    pub(crate) fn continues(&self) -> Option<u64> {
        match self {
            Self::Nudge { prompt, .. } => Some(*prompt),
            _ => None,
        }
    }
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
