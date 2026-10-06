//! One fact observed at a door, and the one record shape it reifies as.
//!
//! The surface rail, the audit trail, `--audit`, and the wire all speak this
//! vocabulary: an [`Observation`] encodes as a record of `site`, `start`,
//! `end` and `principal`, the fact itself `what`, a variant whose tag is the
//! kind, so no separate `kind` field can disagree with the payload beside it.
//! On the surface channel, which carries other classes too, the record rides
//! as `` `observed <record> `` ([`Observation::to_surface`]).  The checker
//! reads its type off the declaration (`Observation::ty`).

use super::{LeaseClass, WorkerId};
use crate::first_order::datum::{Datum, field, tag, untag};
use crate::first_order::{Bytes, FOValue};
use crate::ir::WriteMode;
use crate::source::CallSite;
use crate::ty::{Ty, Typed, closed_record};
use crate::{label, record, variant};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use strum::{IntoStaticStr, VariantArray};

/// One fact observed at a door: a command settled, a write committed, a
/// redirect read opened, a capability check decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// `None` where the observing dispatch has no position in a session source.
    pub site: Option<CallSite>,
    /// Microseconds since the Unix epoch; equal to `end` at an instantaneous
    /// door.
    pub start: i64,
    pub end: i64,
    /// `USER` at the time of observing; `None` where nothing named one.
    pub principal: Option<String>,
    pub what: Observed,
}

record!(typed Observation {
    site: "site",
    start: "start",
    end: "end",
    principal: "principal",
    what: "what",
});

/// What was observed.
///
/// A command carries one fact whether it was an external or a detached spawn;
/// the door it passed through is `origin`.  A builtin application is not one:
/// its effects are observed at the doors they pass through, as the `Write`,
/// `Read`, or `Worker` they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    Command(Command),
    Write(Write),
    Read(Read),
    Grep(Grep),
    Check(Check),
    Worker(Worker),
    Act(Act),
}

variant!(typed Observed {
    Command(Command): "command",
    Write(Write): "write",
    Read(Read): "read",
    Grep(Grep): "grep",
    Check(Check): "check",
    Worker(Worker): "worker",
    Act(Act): "act",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Shown name first, then its arguments.
    pub argv: Vec<String>,
    pub status: i32,
    pub origin: CommandOrigin,
    /// Bytes teed off fd 1 and fd 2, empty unless the capture policy is
    /// `Bytes`.
    pub stdout: Bytes,
    pub stderr: Bytes,
    /// The runtime's own account of why the command failed: `Some` iff the
    /// outcome was a runtime error.  The streams hold only what the child
    /// wrote; this is the one place ral speaks in its own voice.
    pub error: Option<String>,
}

record!(typed Command {
    argv: "argv",
    status: "status",
    origin: "origin",
    stdout: "stdout",
    stderr: "stderr",
    error: "error",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write {
    pub path: String,
    pub mode: WriteMode,
    pub outcome: WriteOutcome,
    /// The target's whole content once the write landed.  Never a prefix:
    /// a card reads a write as the change it made, and half a side is not
    /// a change.
    pub new_bytes: Option<Bytes>,
    /// The target's whole content before the write, empty for a file that
    /// did not yet exist.  `None` means the before-image is *unknown* (a
    /// target too large to read whole), which is not the same fact and must
    /// not read as a creation.
    pub old_bytes: Option<Bytes>,
}

record!(typed Write {
    path: "path",
    mode: "mode",
    outcome: "outcome",
    new_bytes: "new-bytes",
    old_bytes: "old-bytes",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    pub path: String,
}

record!(typed Read { path: "path" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grep {
    pub scope: String,
    pub pattern: String,
}

record!(typed Grep {
    scope: "scope",
    pattern: "pattern",
});

/// A recorded capability check: the resource class judged and its detail.  The
/// decision is the resource's own, so no `Denied` deputy exists to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub resource: Resource,
    /// Per-resource detail, a nested map beside `resource` and `decision`.
    pub fields: BTreeMap<String, String>,
}

impl Check {
    pub fn new(resource: Resource, fields: BTreeMap<String, String>) -> Self {
        Self { resource, fields }
    }

    pub fn decision(&self) -> Decision {
        self.resource.decision()
    }
}

/// Resource, decision and detail, sorted; a record whose decision is not the
/// resource's own is refused.
impl Datum for Check {
    fn encode(self) -> FOValue {
        let decision = self.decision().encode();
        FOValue::Map {
            entries: vec![
                ("decision".into(), decision),
                ("fields".into(), self.fields.encode()),
                ("resource".into(), self.resource.encode()),
            ],
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        crate::first_order::datum::exact_keys(v, &["resource", "decision", "fields"])?;
        let resource: Resource = field(v, "resource")?;
        let decision: Decision = field(v, "decision")?;
        if decision != resource.decision() {
            let (kind, label) = (<&str>::from(resource), <&str>::from(decision));
            return Err(format!("a `{kind} check is never `{label}"));
        }
        Ok(Self::new(resource, field(v, "fields")?))
    }
}

impl Typed for Check {
    fn ty() -> Ty {
        closed_record(&[
            ("resource", Resource::ty()),
            ("decision", Decision::ty()),
            ("fields", BTreeMap::<String, String>::ty()),
        ])
    }
}

/// The resource class a check judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum Resource {
    Exec,
    Fs,
    Deputy,
}

label!(typed Resource);

impl Resource {
    /// An admitted check is never recorded; only a deputy is reported without
    /// being refused.
    pub fn decision(self) -> Decision {
        match self {
            Self::Exec | Self::Fs => Decision::Denied,
            Self::Deputy => Decision::Flagged,
        }
    }
}

/// How a recorded capability check settled.  An admitted check is never
/// recorded, so there is no `Allowed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum Decision {
    Denied,
    /// Reported, not enforced: `capability::deputy_prefixes` names a confused
    /// deputy without refusing it, so the run continues either way.
    Flagged,
}

label!(typed Decision);

/// A worker's birth, filed at `spawn_child` in the same breath as its
/// registry entry.
///
/// After the reservation succeeds, so a spawn the cap refused observes
/// nothing.  The fact a later reader joins against the registry, or the
/// `` `workers `` probe, to ask what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worker {
    pub id: WorkerId,
    pub cmd: String,
    pub class: LeaseClass,
}

record!(typed Worker {
    id: "id",
    cmd: "cmd",
    class: "class",
});

/// A harness act, authored host-side at the arm where its outcome is known,
/// never by the engine.  `subject` is the agent name or schedule label a spawn
/// or nudge names; `None` for a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Act {
    pub verb: String,
    pub subject: Option<String>,
    pub payload: String,
    pub refused: bool,
}

record!(typed Act {
    verb: "verb",
    subject: "subject",
    payload: "payload",
    refused: "refused",
});

/// Which door a command came through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum CommandOrigin {
    External,
    /// A background spawn: the status is 0 by construction, not by
    /// observation, since nothing waits for the child.
    Detached,
}

label!(typed CommandOrigin);

/// How a write door settled, ordered from best to worst.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "kebab-case")]
pub enum WriteOutcome {
    Committed,
    /// The body broke before an atomic `>` committed: its staged temp is
    /// discarded and the target left as it was.  A stream cannot abort.
    Aborted,
    /// The open never succeeded, or the atomic rename failed at commit.
    Failed,
}

label!(typed WriteOutcome);

impl Observation {
    /// The tag an observation carries on the surface channel.
    pub const SURFACE_TAG: &str = "observed";

    /// The record tagged for the surface channel, so a host dispatches on the
    /// tag alone.
    pub fn to_surface(&self) -> FOValue {
        tag(Self::SURFACE_TAG, Some(self.clone().encode()))
    }

    /// Inverse of [`Self::to_surface`]; `None` for any other surface event.
    pub fn from_surface(v: &FOValue) -> Option<Self> {
        match untag(v)? {
            (Self::SURFACE_TAG, Some(record)) => Self::decode(record).ok(),
            _ => None,
        }
    }
}
