//! Complete serialization mirrors; domain checks precede semantic hydration.
use super::{
    AgentId, BlockId, ChildFailure, ChildStatus, Control, Count, CountRule, DecodeCause,
    DecodeFailure, Display, Done, Fields, Forensic, HelperFailure, JobId, JsonEncodeFailure,
    JsonRef, Mirror, Post, PostId, RawCard, RawChange, RawChatMessage, RawDatum, RawEviction,
    RawFOValue, RawLinked, RawNonEmpty, RawPath, RawProviderError, RawSelected, RawSettlement,
    RawSevered, RawSpanRow, RawVia, Reason, Record, Schedule, Serialize, Spent, Started,
    ToolResult, Transcript, Tuning, UnknownField, Usage, Verdict, Voice, WireError, duplicate,
    external, length, missing, on_json_stack, required_payload, unit,
};
#[derive(Debug, Serialize)]
pub(crate) struct RawStarted {
    session_id: AgentId,
    name: String,
    parent: Option<AgentId>,
    returns: bool,
    selected: RawSelected,
    log_dir: RawPath,
    at_unix_ms: u64,
}
impl Mirror for RawStarted {
    type Value = Started;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, true, Self, { session_id: AgentId => required, name: String => required, parent: Option<AgentId> => optional, returns: bool => required, selected: RawSelected => required, log_dir: RawPath => required, at_unix_ms: u64 => required })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.session_id.check().map_err(|e| e.field("session_id"))?;
        self.name.check().map_err(|e| e.field("name"))?;
        self.parent.check().map_err(|e| e.field("parent"))?;
        self.returns.check().map_err(|e| e.field("returns"))?;
        self.selected.check().map_err(|e| e.field("selected"))?;
        self.log_dir.check().map_err(|e| e.field("log_dir"))?;
        self.at_unix_ms.check().map_err(|e| e.field("at_unix_ms"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        Started {
            session_id: self.session_id.hydrate(),
            name: self.name.hydrate(),
            parent: self.parent.hydrate(),
            returns: self.returns.hydrate(),
            selected: self.selected.hydrate(),
            log_dir: self.log_dir.hydrate(),
            at_unix_ms: self.at_unix_ms.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            session_id: <AgentId as Mirror>::from_value(&value.session_id)
                .map_err(|e| e.field("session_id"))?,
            name: <String as Mirror>::from_value(&value.name).map_err(|e| e.field("name"))?,
            parent: <Option<AgentId> as Mirror>::from_value(&value.parent)
                .map_err(|e| e.field("parent"))?,
            returns: <bool as Mirror>::from_value(&value.returns)
                .map_err(|e| e.field("returns"))?,
            selected: <RawSelected as Mirror>::from_value(&value.selected)
                .map_err(|e| e.field("selected"))?,
            log_dir: <RawPath as Mirror>::from_value(&value.log_dir)
                .map_err(|e| e.field("log_dir"))?,
            at_unix_ms: <u64 as Mirror>::from_value(&value.at_unix_ms)
                .map_err(|e| e.field("at_unix_ms"))?,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RawWireError {
    reason: RawDatum<Reason>,
    context: Option<String>,
    hints: Vec<String>,
}
impl Mirror for RawWireError {
    type Value = WireError;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, false, Self, { reason: RawDatum<Reason> => required, context: Option<String> => optional, hints: Vec<String> => required })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.reason.check().map_err(|e| e.field("reason"))?;
        self.context.check().map_err(|e| e.field("context"))?;
        self.hints.check().map_err(|e| e.field("hints"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        WireError {
            reason: self.reason.hydrate(),
            context: self.context.hydrate(),
            hints: self.hints.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            reason: <RawDatum<Reason> as Mirror>::from_value(&value.reason)
                .map_err(|e| e.field("reason"))?,
            context: <Option<String> as Mirror>::from_value(&value.context)
                .map_err(|e| e.field("context"))?,
            hints: <Vec<String> as Mirror>::from_value(&value.hints)
                .map_err(|e| e.field("hints"))?,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) enum RawRecord {
    Transcript(RawTranscript),
    Control(RawControl),
    Display(RawDisplay),
    Forensic(RawForensic),
}
impl Mirror for RawRecord {
    type Value = Record;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        on_json_stack(|| {
            let (tag, payload) =
                external(value, &["Transcript", "Control", "Display", "Forensic"])?;
            match tag {
                "Transcript" => <RawTranscript>::read(required_payload(payload, tag)?)
                    .map(Self::Transcript)
                    .map_err(|e| e.payload(tag)),
                "Control" => <RawControl>::read(required_payload(payload, tag)?)
                    .map(Self::Control)
                    .map_err(|e| e.payload(tag)),
                "Display" => <RawDisplay>::read(required_payload(payload, tag)?)
                    .map(Self::Display)
                    .map_err(|e| e.payload(tag)),
                "Forensic" => <RawForensic>::read(required_payload(payload, tag)?)
                    .map(Self::Forensic)
                    .map_err(|e| e.payload(tag)),
                _ => unreachable!("external checked tag"),
            }
        })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        on_json_stack(|| match self {
            Self::Transcript(value) => value.check().map_err(|e| e.payload("Transcript")),
            Self::Control(value) => value.check().map_err(|e| e.payload("Control")),
            Self::Display(value) => value.check().map_err(|e| e.payload("Display")),
            Self::Forensic(value) => value.check().map_err(|e| e.payload("Forensic")),
        })
    }
    fn hydrate(&self) -> Self::Value {
        on_json_stack(|| match self {
            Self::Transcript(value) => Record::Transcript(value.hydrate()),
            Self::Control(value) => Record::Control(value.hydrate()),
            Self::Display(value) => Record::Display(value.hydrate()),
            Self::Forensic(value) => Record::Forensic(value.hydrate()),
        })
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        on_json_stack(|| match value {
            Record::Transcript(value) => <RawTranscript as Mirror>::from_value(value)
                .map(Self::Transcript)
                .map_err(|e| e.payload("Transcript")),
            Record::Control(value) => <RawControl as Mirror>::from_value(value)
                .map(Self::Control)
                .map_err(|e| e.payload("Control")),
            Record::Display(value) => <RawDisplay as Mirror>::from_value(value)
                .map(Self::Display)
                .map_err(|e| e.payload("Display")),
            Record::Forensic(value) => <RawForensic as Mirror>::from_value(value)
                .map(Self::Forensic)
                .map_err(|e| e.payload("Forensic")),
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RawControl {
    SessionStarted(RawStarted),
    SessionResumed {
        selected: RawSelected,
        returns: bool,
        at_unix_ms: u64,
    },
    JobOpened,
    JobSettled {
        job: JobId,
        stopped: bool,
    },
    Status {
        status: RawChildStatus,
    },
    Posted(Post),
    Taken {
        posts: RawNonEmpty<PostId>,
    },
    Withdrawn {
        posts: RawNonEmpty<PostId>,
    },
    Scheduled {
        label: String,
        schedule: Schedule,
    },
    Unscheduled {
        label: String,
    },
    ModelChanged {
        selected: RawSelected,
    },
    WindowNarrowed {
        model: String,
        context_window: u64,
    },
}
impl Mirror for RawControl {
    type Value = Control;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        on_json_stack(|| {
            let (tag, payload) = external(
                value,
                &[
                    "session_started",
                    "session_resumed",
                    "job_opened",
                    "job_settled",
                    "status",
                    "posted",
                    "taken",
                    "withdrawn",
                    "scheduled",
                    "unscheduled",
                    "model_changed",
                    "window_narrowed",
                ],
            )?;
            match tag {
                "session_started" => <RawStarted>::read(required_payload(payload, tag)?)
                    .map(Self::SessionStarted)
                    .map_err(|e| e.payload(tag)),
                "session_resumed" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::SessionResumed, { selected: RawSelected => required, returns: bool => required, at_unix_ms: u64 => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "job_opened" => unit(payload, tag).map(|()| Self::JobOpened),
                "job_settled" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::JobSettled, { job: JobId => required, stopped: bool => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "status" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Status, { status: RawChildStatus => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "posted" => <Post>::read(required_payload(payload, tag)?)
                    .map(Self::Posted)
                    .map_err(|e| e.payload(tag)),
                "taken" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Taken, { posts: RawNonEmpty<PostId> => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "withdrawn" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Withdrawn, { posts: RawNonEmpty<PostId> => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "scheduled" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Scheduled, { label: String => required, schedule: Schedule => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "unscheduled" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Unscheduled, { label: String => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "model_changed" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::ModelChanged, { selected: RawSelected => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "window_narrowed" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::WindowNarrowed, { model: String => required, context_window: u64 => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                _ => unreachable!("external checked tag"),
            }
        })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        on_json_stack(|| match self {
            Self::SessionStarted(value) => value.check().map_err(|e| e.payload("session_started")),
            Self::SessionResumed {
                selected,
                returns,
                at_unix_ms,
            } => (|| {
                selected.check().map_err(|e| e.field("selected"))?;
                returns.check().map_err(|e| e.field("returns"))?;
                at_unix_ms.check().map_err(|e| e.field("at_unix_ms"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("session_resumed")),
            Self::JobOpened => Ok(()),
            Self::JobSettled { job, stopped } => (|| {
                job.check().map_err(|e| e.field("job"))?;
                stopped.check().map_err(|e| e.field("stopped"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("job_settled")),
            Self::Status { status } => (|| {
                status.check().map_err(|e| e.field("status"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("status")),
            Self::Posted(value) => value.check().map_err(|e| e.payload("posted")),
            Self::Taken { posts } => (|| {
                posts.check().map_err(|e| e.field("posts"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("taken")),
            Self::Withdrawn { posts } => (|| {
                posts.check().map_err(|e| e.field("posts"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("withdrawn")),
            Self::Scheduled { label, schedule } => (|| {
                label.check().map_err(|e| e.field("label"))?;
                schedule.check().map_err(|e| e.field("schedule"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("scheduled")),
            Self::Unscheduled { label } => (|| {
                label.check().map_err(|e| e.field("label"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("unscheduled")),
            Self::ModelChanged { selected } => (|| {
                selected.check().map_err(|e| e.field("selected"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("model_changed")),
            Self::WindowNarrowed {
                model,
                context_window,
            } => (|| {
                model.check().map_err(|e| e.field("model"))?;
                context_window
                    .check()
                    .map_err(|e| e.field("context_window"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("window_narrowed")),
        })
    }
    fn hydrate(&self) -> Self::Value {
        on_json_stack(|| match self {
            Self::SessionStarted(value) => Control::SessionStarted(value.hydrate()),
            Self::SessionResumed {
                selected,
                returns,
                at_unix_ms,
            } => Control::SessionResumed {
                selected: selected.hydrate(),
                returns: returns.hydrate(),
                at_unix_ms: at_unix_ms.hydrate(),
            },
            Self::JobOpened => Control::JobOpened,
            Self::JobSettled { job, stopped } => Control::JobSettled {
                job: job.hydrate(),
                stopped: stopped.hydrate(),
            },
            Self::Status { status } => Control::Status {
                status: status.hydrate(),
            },
            Self::Posted(value) => Control::Posted(value.hydrate()),
            Self::Taken { posts } => Control::Taken {
                posts: posts.hydrate(),
            },
            Self::Withdrawn { posts } => Control::Withdrawn {
                posts: posts.hydrate(),
            },
            Self::Scheduled { label, schedule } => Control::Scheduled {
                label: label.hydrate(),
                schedule: schedule.hydrate(),
            },
            Self::Unscheduled { label } => Control::Unscheduled {
                label: label.hydrate(),
            },
            Self::ModelChanged { selected } => Control::ModelChanged {
                selected: selected.hydrate(),
            },
            Self::WindowNarrowed {
                model,
                context_window,
            } => Control::WindowNarrowed {
                model: model.hydrate(),
                context_window: context_window.hydrate(),
            },
        })
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        on_json_stack(|| match value {
            Control::SessionStarted(value) => <RawStarted as Mirror>::from_value(value)
                .map(Self::SessionStarted)
                .map_err(|e| e.payload("session_started")),
            Control::SessionResumed {
                selected,
                returns,
                at_unix_ms,
            } => (|| {
                Ok(Self::SessionResumed {
                    selected: <RawSelected as Mirror>::from_value(selected)
                        .map_err(|e| e.field("selected"))?,
                    returns: <bool as Mirror>::from_value(returns)
                        .map_err(|e| e.field("returns"))?,
                    at_unix_ms: <u64 as Mirror>::from_value(at_unix_ms)
                        .map_err(|e| e.field("at_unix_ms"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("session_resumed")),
            Control::JobOpened => Ok(Self::JobOpened),
            Control::JobSettled { job, stopped } => (|| {
                Ok(Self::JobSettled {
                    job: <JobId as Mirror>::from_value(job).map_err(|e| e.field("job"))?,
                    stopped: <bool as Mirror>::from_value(stopped)
                        .map_err(|e| e.field("stopped"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("job_settled")),
            Control::Status { status } => (|| {
                Ok(Self::Status {
                    status: <RawChildStatus as Mirror>::from_value(status)
                        .map_err(|e| e.field("status"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("status")),
            Control::Posted(value) => <Post as Mirror>::from_value(value)
                .map(Self::Posted)
                .map_err(|e| e.payload("posted")),
            Control::Taken { posts } => (|| {
                Ok(Self::Taken {
                    posts: <RawNonEmpty<PostId> as Mirror>::from_value(posts)
                        .map_err(|e| e.field("posts"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("taken")),
            Control::Withdrawn { posts } => (|| {
                Ok(Self::Withdrawn {
                    posts: <RawNonEmpty<PostId> as Mirror>::from_value(posts)
                        .map_err(|e| e.field("posts"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("withdrawn")),
            Control::Scheduled { label, schedule } => (|| {
                Ok(Self::Scheduled {
                    label: <String as Mirror>::from_value(label).map_err(|e| e.field("label"))?,
                    schedule: <Schedule as Mirror>::from_value(schedule)
                        .map_err(|e| e.field("schedule"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("scheduled")),
            Control::Unscheduled { label } => (|| {
                Ok(Self::Unscheduled {
                    label: <String as Mirror>::from_value(label).map_err(|e| e.field("label"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("unscheduled")),
            Control::ModelChanged { selected } => (|| {
                Ok(Self::ModelChanged {
                    selected: <RawSelected as Mirror>::from_value(selected)
                        .map_err(|e| e.field("selected"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("model_changed")),
            Control::WindowNarrowed {
                model,
                context_window,
            } => (|| {
                Ok(Self::WindowNarrowed {
                    model: <String as Mirror>::from_value(model).map_err(|e| e.field("model"))?,
                    context_window: <u64 as Mirror>::from_value(context_window)
                        .map_err(|e| e.field("context_window"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("window_narrowed")),
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RawTranscript {
    System {
        text: String,
    },
    UserPrompt {
        turn: u64,
        text: String,
        voice: Voice,
    },
    Steering {
        text: String,
        voice: Voice,
    },
    Import {
        turn: u64,
        message: RawChatMessage,
    },
    Inherited {
        turns: Vec<RawLinked>,
        evictions: Vec<RawEviction>,
    },
    AssistantMessage {
        turn: u64,
        message: RawChatMessage,
        pending_tool_ids: Vec<String>,
        stop_reason: Option<String>,
    },
    ToolResults {
        results: Vec<ToolResult>,
    },
    Evicted(RawEviction),
    Rewound {
        anchor: u64,
    },
    Cleared,
}
impl Mirror for RawTranscript {
    type Value = Transcript;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        on_json_stack(|| {
            let (tag, payload) = external(
                value,
                &[
                    "system",
                    "user_prompt",
                    "steering",
                    "import",
                    "inherited",
                    "assistant_message",
                    "tool_results",
                    "evicted",
                    "rewound",
                    "cleared",
                ],
            )?;
            match tag {
                "system" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::System, { text: String => required }))()
                        .map_err(|e: DecodeFailure| e.payload(tag))
                }
                "user_prompt" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::UserPrompt, { turn: u64 => required, text: String => required, voice: Voice => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "steering" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Steering, { text: String => required, voice: Voice => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "import" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Import, { turn: u64 => required, message: RawChatMessage => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "inherited" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Inherited, { turns: Vec<RawLinked> => required, evictions: Vec<RawEviction> => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "assistant_message" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::AssistantMessage, { turn: u64 => required, message: RawChatMessage => required, pending_tool_ids: Vec<String> => required, stop_reason: Option<String> => optional }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "tool_results" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::ToolResults, { results: Vec<ToolResult> => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "evicted" => <RawEviction>::read(required_payload(payload, tag)?)
                    .map(Self::Evicted)
                    .map_err(|e| e.payload(tag)),
                "rewound" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Rewound, { anchor: u64 => required }))()
                        .map_err(|e: DecodeFailure| e.payload(tag))
                }
                "cleared" => unit(payload, tag).map(|()| Self::Cleared),
                _ => unreachable!("external checked tag"),
            }
        })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        on_json_stack(|| match self {
            Self::System { text } => (|| {
                text.check().map_err(|e| e.field("text"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("system")),
            Self::UserPrompt { turn, text, voice } => (|| {
                turn.check().map_err(|e| e.field("turn"))?;
                text.check().map_err(|e| e.field("text"))?;
                voice.check().map_err(|e| e.field("voice"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("user_prompt")),
            Self::Steering { text, voice } => (|| {
                text.check().map_err(|e| e.field("text"))?;
                voice.check().map_err(|e| e.field("voice"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("steering")),
            Self::Import { turn, message } => (|| {
                turn.check().map_err(|e| e.field("turn"))?;
                message.check().map_err(|e| e.field("message"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("import")),
            Self::Inherited { turns, evictions } => (|| {
                turns.check().map_err(|e| e.field("turns"))?;
                evictions.check().map_err(|e| e.field("evictions"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("inherited")),
            Self::AssistantMessage {
                turn,
                message,
                pending_tool_ids,
                stop_reason,
            } => (|| {
                turn.check().map_err(|e| e.field("turn"))?;
                message.check().map_err(|e| e.field("message"))?;
                pending_tool_ids
                    .check()
                    .map_err(|e| e.field("pending_tool_ids"))?;
                stop_reason.check().map_err(|e| e.field("stop_reason"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("assistant_message")),
            Self::ToolResults { results } => (|| {
                results.check().map_err(|e| e.field("results"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("tool_results")),
            Self::Evicted(value) => value.check().map_err(|e| e.payload("evicted")),
            Self::Rewound { anchor } => (|| {
                anchor.check().map_err(|e| e.field("anchor"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("rewound")),
            Self::Cleared => Ok(()),
        })
    }
    fn hydrate(&self) -> Self::Value {
        on_json_stack(|| match self {
            Self::System { text } => Transcript::System {
                text: text.hydrate(),
            },
            Self::UserPrompt { turn, text, voice } => Transcript::UserPrompt {
                turn: turn.hydrate(),
                text: text.hydrate(),
                voice: voice.hydrate(),
            },
            Self::Steering { text, voice } => Transcript::Steering {
                text: text.hydrate(),
                voice: voice.hydrate(),
            },
            Self::Import { turn, message } => Transcript::Import {
                turn: turn.hydrate(),
                message: message.hydrate(),
            },
            Self::Inherited { turns, evictions } => Transcript::Inherited {
                turns: turns.hydrate(),
                evictions: evictions.hydrate(),
            },
            Self::AssistantMessage {
                turn,
                message,
                pending_tool_ids,
                stop_reason,
            } => Transcript::AssistantMessage {
                turn: turn.hydrate(),
                message: message.hydrate(),
                pending_tool_ids: pending_tool_ids.hydrate(),
                stop_reason: stop_reason.hydrate(),
            },
            Self::ToolResults { results } => Transcript::ToolResults {
                results: results.hydrate(),
            },
            Self::Evicted(value) => Transcript::Evicted(value.hydrate()),
            Self::Rewound { anchor } => Transcript::Rewound {
                anchor: anchor.hydrate(),
            },
            Self::Cleared => Transcript::Cleared,
        })
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        on_json_stack(|| match value {
            Transcript::System { text } => (|| {
                Ok(Self::System {
                    text: <String as Mirror>::from_value(text).map_err(|e| e.field("text"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("system")),
            Transcript::UserPrompt { turn, text, voice } => (|| {
                Ok(Self::UserPrompt {
                    turn: <u64 as Mirror>::from_value(turn).map_err(|e| e.field("turn"))?,
                    text: <String as Mirror>::from_value(text).map_err(|e| e.field("text"))?,
                    voice: <Voice as Mirror>::from_value(voice).map_err(|e| e.field("voice"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("user_prompt")),
            Transcript::Steering { text, voice } => (|| {
                Ok(Self::Steering {
                    text: <String as Mirror>::from_value(text).map_err(|e| e.field("text"))?,
                    voice: <Voice as Mirror>::from_value(voice).map_err(|e| e.field("voice"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("steering")),
            Transcript::Import { turn, message } => (|| {
                Ok(Self::Import {
                    turn: <u64 as Mirror>::from_value(turn).map_err(|e| e.field("turn"))?,
                    message: <RawChatMessage as Mirror>::from_value(message)
                        .map_err(|e| e.field("message"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("import")),
            Transcript::Inherited { turns, evictions } => (|| {
                Ok(Self::Inherited {
                    turns: <Vec<RawLinked> as Mirror>::from_value(turns)
                        .map_err(|e| e.field("turns"))?,
                    evictions: <Vec<RawEviction> as Mirror>::from_value(evictions)
                        .map_err(|e| e.field("evictions"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("inherited")),
            Transcript::AssistantMessage {
                turn,
                message,
                pending_tool_ids,
                stop_reason,
            } => (|| {
                Ok(Self::AssistantMessage {
                    turn: <u64 as Mirror>::from_value(turn).map_err(|e| e.field("turn"))?,
                    message: <RawChatMessage as Mirror>::from_value(message)
                        .map_err(|e| e.field("message"))?,
                    pending_tool_ids: <Vec<String> as Mirror>::from_value(pending_tool_ids)
                        .map_err(|e| e.field("pending_tool_ids"))?,
                    stop_reason: <Option<String> as Mirror>::from_value(stop_reason)
                        .map_err(|e| e.field("stop_reason"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("assistant_message")),
            Transcript::ToolResults { results } => (|| {
                Ok(Self::ToolResults {
                    results: <Vec<ToolResult> as Mirror>::from_value(results)
                        .map_err(|e| e.field("results"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("tool_results")),
            Transcript::Evicted(value) => <RawEviction as Mirror>::from_value(value)
                .map(Self::Evicted)
                .map_err(|e| e.payload("evicted")),
            Transcript::Rewound { anchor } => (|| {
                Ok(Self::Rewound {
                    anchor: <u64 as Mirror>::from_value(anchor).map_err(|e| e.field("anchor"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("rewound")),
            Transcript::Cleared => Ok(Self::Cleared),
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RawChildFailure {
    Cause(RawDatum<HelperFailure>),
    Refused(RawWireError),
    Panicked(String),
}
impl Mirror for RawChildFailure {
    type Value = ChildFailure;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        on_json_stack(|| {
            let (tag, payload) = external(value, &["cause", "refused", "panicked"])?;
            match tag {
                "cause" => <RawDatum<HelperFailure>>::read(required_payload(payload, tag)?)
                    .map(Self::Cause)
                    .map_err(|e| e.payload(tag)),
                "refused" => <RawWireError>::read(required_payload(payload, tag)?)
                    .map(Self::Refused)
                    .map_err(|e| e.payload(tag)),
                "panicked" => <String>::read(required_payload(payload, tag)?)
                    .map(Self::Panicked)
                    .map_err(|e| e.payload(tag)),
                _ => unreachable!("external checked tag"),
            }
        })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        on_json_stack(|| match self {
            Self::Cause(value) => value.check().map_err(|e| e.payload("cause")),
            Self::Refused(value) => value.check().map_err(|e| e.payload("refused")),
            Self::Panicked(value) => value.check().map_err(|e| e.payload("panicked")),
        })
    }
    fn hydrate(&self) -> Self::Value {
        on_json_stack(|| match self {
            Self::Cause(value) => ChildFailure::Cause(value.hydrate()),
            Self::Refused(value) => ChildFailure::Refused(value.hydrate()),
            Self::Panicked(value) => ChildFailure::Panicked(value.hydrate()),
        })
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        on_json_stack(|| match value {
            ChildFailure::Cause(value) => <RawDatum<HelperFailure> as Mirror>::from_value(value)
                .map(Self::Cause)
                .map_err(|e| e.payload("cause")),
            ChildFailure::Refused(value) => <RawWireError as Mirror>::from_value(value)
                .map(Self::Refused)
                .map_err(|e| e.payload("refused")),
            ChildFailure::Panicked(value) => <String as Mirror>::from_value(value)
                .map(Self::Panicked)
                .map_err(|e| e.payload("panicked")),
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RawChildStatus {
    Replied(RawFOValue),
    Failed(RawChildFailure),
    Stopped,
}
impl Mirror for RawChildStatus {
    type Value = ChildStatus;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        on_json_stack(|| {
            let (tag, payload) = external(value, &["replied", "failed", "stopped"])?;
            match tag {
                "replied" => <RawFOValue>::read(required_payload(payload, tag)?)
                    .map(Self::Replied)
                    .map_err(|e| e.payload(tag)),
                "failed" => <RawChildFailure>::read(required_payload(payload, tag)?)
                    .map(Self::Failed)
                    .map_err(|e| e.payload(tag)),
                "stopped" => unit(payload, tag).map(|()| Self::Stopped),
                _ => unreachable!("external checked tag"),
            }
        })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        on_json_stack(|| match self {
            Self::Replied(value) => value.check().map_err(|e| e.payload("replied")),
            Self::Failed(value) => value.check().map_err(|e| e.payload("failed")),
            Self::Stopped => Ok(()),
        })
    }
    fn hydrate(&self) -> Self::Value {
        on_json_stack(|| match self {
            Self::Replied(value) => ChildStatus::Replied(value.hydrate()),
            Self::Failed(value) => ChildStatus::Failed(value.hydrate()),
            Self::Stopped => ChildStatus::Stopped,
        })
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        on_json_stack(|| match value {
            ChildStatus::Replied(value) => <RawFOValue as Mirror>::from_value(value)
                .map(Self::Replied)
                .map_err(|e| e.payload("replied")),
            ChildStatus::Failed(value) => <RawChildFailure as Mirror>::from_value(value)
                .map(Self::Failed)
                .map_err(|e| e.payload("failed")),
            ChildStatus::Stopped => Ok(Self::Stopped),
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RawDisplay {
    Thinking {
        text: String,
    },
    Answer {
        text: String,
    },
    ToolCall {
        tool: String,
        cmd: String,
        summary: Option<String>,
    },
    HarnessCall {
        verb: String,
        subject: Option<String>,
        payload: String,
        failed: bool,
    },
    Verdict {
        call: BlockId,
        verdict: Verdict,
    },
    SubagentDone {
        name: String,
        outcome: RawSettlement,
        elapsed_ms: u64,
    },
    Observation {
        value: RawFOValue,
    },
    Change {
        change: RawChange,
    },
    Card {
        card: RawCard,
    },
    Done {
        id: Option<u64>,
        title: String,
        elapsed_ms: Option<u64>,
        outcome: RawDatum<Done>,
    },
    Context {
        rows: Vec<RawSpanRow>,
    },
}
impl Mirror for RawDisplay {
    type Value = Display;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        on_json_stack(|| {
            let (tag, payload) = external(
                value,
                &[
                    "thinking",
                    "answer",
                    "tool_call",
                    "harness_call",
                    "verdict",
                    "subagent_done",
                    "observation",
                    "change",
                    "card",
                    "done",
                    "context",
                ],
            )?;
            match tag {
                "thinking" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Thinking, { text: String => required }))()
                        .map_err(|e: DecodeFailure| e.payload(tag))
                }
                "answer" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Answer, { text: String => required }))()
                        .map_err(|e: DecodeFailure| e.payload(tag))
                }
                "tool_call" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::ToolCall, { tool: String => required, cmd: String => required, summary: Option<String> => optional }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "harness_call" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::HarnessCall, { verb: String => required, subject: Option<String> => optional, payload: String => required, failed: bool => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "verdict" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Verdict, { call: BlockId => required, verdict: Verdict => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "subagent_done" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::SubagentDone, { name: String => required, outcome: RawSettlement => required, elapsed_ms: u64 => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "observation" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Observation, { value: RawFOValue => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "change" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Change, { change: RawChange => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "card" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Card, { card: RawCard => required }))()
                        .map_err(|e: DecodeFailure| e.payload(tag))
                }
                "done" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Done, { id: Option<u64> => optional, title: String => required, elapsed_ms: Option<u64> => optional, outcome: RawDatum<Done> => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "context" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Context, { rows: Vec<RawSpanRow> => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                _ => unreachable!("external checked tag"),
            }
        })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        on_json_stack(|| match self {
            Self::Thinking { text } => (|| {
                text.check().map_err(|e| e.field("text"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("thinking")),
            Self::Answer { text } => (|| {
                text.check().map_err(|e| e.field("text"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("answer")),
            Self::ToolCall { tool, cmd, summary } => (|| {
                tool.check().map_err(|e| e.field("tool"))?;
                cmd.check().map_err(|e| e.field("cmd"))?;
                summary.check().map_err(|e| e.field("summary"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("tool_call")),
            Self::HarnessCall {
                verb,
                subject,
                payload,
                failed,
            } => (|| {
                verb.check().map_err(|e| e.field("verb"))?;
                subject.check().map_err(|e| e.field("subject"))?;
                payload.check().map_err(|e| e.field("payload"))?;
                failed.check().map_err(|e| e.field("failed"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("harness_call")),
            Self::Verdict { call, verdict } => (|| {
                call.check().map_err(|e| e.field("call"))?;
                verdict.check().map_err(|e| e.field("verdict"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("verdict")),
            Self::SubagentDone {
                name,
                outcome,
                elapsed_ms,
            } => (|| {
                name.check().map_err(|e| e.field("name"))?;
                outcome.check().map_err(|e| e.field("outcome"))?;
                elapsed_ms.check().map_err(|e| e.field("elapsed_ms"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("subagent_done")),
            Self::Observation { value } => (|| {
                value.check().map_err(|e| e.field("value"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("observation")),
            Self::Change { change } => (|| {
                change.check().map_err(|e| e.field("change"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("change")),
            Self::Card { card } => (|| {
                card.check().map_err(|e| e.field("card"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("card")),
            Self::Done {
                id,
                title,
                elapsed_ms,
                outcome,
            } => (|| {
                id.check().map_err(|e| e.field("id"))?;
                title.check().map_err(|e| e.field("title"))?;
                elapsed_ms.check().map_err(|e| e.field("elapsed_ms"))?;
                outcome.check().map_err(|e| e.field("outcome"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("done")),
            Self::Context { rows } => (|| {
                rows.check().map_err(|e| e.field("rows"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("context")),
        })
    }
    fn hydrate(&self) -> Self::Value {
        on_json_stack(|| match self {
            Self::Thinking { text } => Display::Thinking {
                text: text.hydrate(),
            },
            Self::Answer { text } => Display::Answer {
                text: text.hydrate(),
            },
            Self::ToolCall { tool, cmd, summary } => Display::ToolCall {
                tool: tool.hydrate(),
                cmd: cmd.hydrate(),
                summary: summary.hydrate(),
            },
            Self::HarnessCall {
                verb,
                subject,
                payload,
                failed,
            } => Display::HarnessCall {
                verb: verb.hydrate(),
                subject: subject.hydrate(),
                payload: payload.hydrate(),
                failed: failed.hydrate(),
            },
            Self::Verdict { call, verdict } => Display::Verdict {
                call: call.hydrate(),
                verdict: verdict.hydrate(),
            },
            Self::SubagentDone {
                name,
                outcome,
                elapsed_ms,
            } => Display::SubagentDone {
                name: name.hydrate(),
                outcome: outcome.hydrate(),
                elapsed_ms: elapsed_ms.hydrate(),
            },
            Self::Observation { value } => Display::Observation {
                value: value.hydrate(),
            },
            Self::Change { change } => Display::Change {
                change: change.hydrate(),
            },
            Self::Card { card } => Display::Card {
                card: card.hydrate(),
            },
            Self::Done {
                id,
                title,
                elapsed_ms,
                outcome,
            } => Display::Done {
                id: id.hydrate(),
                title: title.hydrate(),
                elapsed_ms: elapsed_ms.hydrate(),
                outcome: outcome.hydrate(),
            },
            Self::Context { rows } => Display::Context {
                rows: rows.hydrate(),
            },
        })
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        on_json_stack(|| match value {
            Display::Thinking { text } => (|| {
                Ok(Self::Thinking {
                    text: <String as Mirror>::from_value(text).map_err(|e| e.field("text"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("thinking")),
            Display::Answer { text } => (|| {
                Ok(Self::Answer {
                    text: <String as Mirror>::from_value(text).map_err(|e| e.field("text"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("answer")),
            Display::ToolCall { tool, cmd, summary } => (|| {
                Ok(Self::ToolCall {
                    tool: <String as Mirror>::from_value(tool).map_err(|e| e.field("tool"))?,
                    cmd: <String as Mirror>::from_value(cmd).map_err(|e| e.field("cmd"))?,
                    summary: <Option<String> as Mirror>::from_value(summary)
                        .map_err(|e| e.field("summary"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("tool_call")),
            Display::HarnessCall {
                verb,
                subject,
                payload,
                failed,
            } => (|| {
                Ok(Self::HarnessCall {
                    verb: <String as Mirror>::from_value(verb).map_err(|e| e.field("verb"))?,
                    subject: <Option<String> as Mirror>::from_value(subject)
                        .map_err(|e| e.field("subject"))?,
                    payload: <String as Mirror>::from_value(payload)
                        .map_err(|e| e.field("payload"))?,
                    failed: <bool as Mirror>::from_value(failed).map_err(|e| e.field("failed"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("harness_call")),
            Display::Verdict { call, verdict } => (|| {
                Ok(Self::Verdict {
                    call: <BlockId as Mirror>::from_value(call).map_err(|e| e.field("call"))?,
                    verdict: <Verdict as Mirror>::from_value(verdict)
                        .map_err(|e| e.field("verdict"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("verdict")),
            Display::SubagentDone {
                name,
                outcome,
                elapsed_ms,
            } => (|| {
                Ok(Self::SubagentDone {
                    name: <String as Mirror>::from_value(name).map_err(|e| e.field("name"))?,
                    outcome: <RawSettlement as Mirror>::from_value(outcome)
                        .map_err(|e| e.field("outcome"))?,
                    elapsed_ms: <u64 as Mirror>::from_value(elapsed_ms)
                        .map_err(|e| e.field("elapsed_ms"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("subagent_done")),
            Display::Observation { value } => (|| {
                Ok(Self::Observation {
                    value: <RawFOValue as Mirror>::from_value(value)
                        .map_err(|e| e.field("value"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("observation")),
            Display::Change { change } => (|| {
                Ok(Self::Change {
                    change: <RawChange as Mirror>::from_value(change)
                        .map_err(|e| e.field("change"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("change")),
            Display::Card { card } => (|| {
                Ok(Self::Card {
                    card: <RawCard as Mirror>::from_value(card).map_err(|e| e.field("card"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("card")),
            Display::Done {
                id,
                title,
                elapsed_ms,
                outcome,
            } => (|| {
                Ok(Self::Done {
                    id: <Option<u64> as Mirror>::from_value(id).map_err(|e| e.field("id"))?,
                    title: <String as Mirror>::from_value(title).map_err(|e| e.field("title"))?,
                    elapsed_ms: <Option<u64> as Mirror>::from_value(elapsed_ms)
                        .map_err(|e| e.field("elapsed_ms"))?,
                    outcome: <RawDatum<Done> as Mirror>::from_value(outcome)
                        .map_err(|e| e.field("outcome"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("done")),
            Display::Context { rows } => (|| {
                Ok(Self::Context {
                    rows: <Vec<RawSpanRow> as Mirror>::from_value(rows)
                        .map_err(|e| e.field("rows"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("context")),
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RawForensic {
    SessionEnded,
    SessionClosed,
    EngineSevered {
        cause: RawSevered,
    },
    TeardownFailed {
        failure: String,
    },
    TurnStarted {
        turn: u64,
        tuning: Tuning,
        #[serde(skip_serializing_if = "Option::is_none")]
        via: Option<RawVia>,
    },
    UsageDelta {
        usage: Usage,
        #[serde(skip_serializing_if = "Option::is_none")]
        generation: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    Cancelled,
    Error {
        text: String,
    },
    Nudge {
        cause: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        spent: Option<Spent>,
    },
    ProviderError {
        error: RawProviderError,
    },
    Retried {
        error: RawProviderError,
    },
    SystemNote {
        text: String,
    },
    HarnessResult {
        text: String,
    },
    Pin {
        key: String,
    },
    Unpin {
        key: String,
    },
    Reap {
        title: String,
        cause: String,
    },
    Prune {
        names: Vec<String>,
        idle_calls: Vec<u64>,
    },
}
impl Mirror for RawForensic {
    type Value = Forensic;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        on_json_stack(|| {
            let (tag, payload) = external(
                value,
                &[
                    "session_ended",
                    "session_closed",
                    "engine_severed",
                    "teardown_failed",
                    "turn_started",
                    "usage_delta",
                    "cancelled",
                    "error",
                    "nudge",
                    "provider_error",
                    "retried",
                    "system_note",
                    "harness_result",
                    "pin",
                    "unpin",
                    "reap",
                    "prune",
                ],
            )?;
            match tag {
                "session_ended" => unit(payload, tag).map(|()| Self::SessionEnded),
                "session_closed" => unit(payload, tag).map(|()| Self::SessionClosed),
                "engine_severed" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::EngineSevered, { cause: RawSevered => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "teardown_failed" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::TeardownFailed, { failure: String => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "turn_started" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::TurnStarted, { turn: u64 => required, tuning: Tuning => required, via: Option<RawVia> => optional }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "usage_delta" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::UsageDelta, { usage: Usage => required, generation: Option<String> => optional, provider: Option<String> => optional }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "cancelled" => unit(payload, tag).map(|()| Self::Cancelled),
                "error" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Error, { text: String => required }))()
                        .map_err(|e: DecodeFailure| e.payload(tag))
                }
                "nudge" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Nudge, { cause: String => required, spent: Option<Spent> => optional }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "provider_error" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::ProviderError, { error: RawProviderError => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "retried" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Retried, { error: RawProviderError => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "system_note" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::SystemNote, { text: String => required }))(
                    )
                    .map_err(|e: DecodeFailure| e.payload(tag))
                }
                "harness_result" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::HarnessResult, { text: String => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "pin" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Pin, { key: String => required }))()
                        .map_err(|e: DecodeFailure| e.payload(tag))
                }
                "unpin" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Unpin, { key: String => required }))()
                        .map_err(|e: DecodeFailure| e.payload(tag))
                }
                "reap" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Reap, { title: String => required, cause: String => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "prune" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, true, Self::Prune, { names: Vec<String> => required, idle_calls: Vec<u64> => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                _ => unreachable!("external checked tag"),
            }
        })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        on_json_stack(|| match self {
            Self::SessionEnded | Self::SessionClosed | Self::Cancelled => Ok(()),
            Self::EngineSevered { cause } => (|| {
                cause.check().map_err(|e| e.field("cause"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("engine_severed")),
            Self::TeardownFailed { failure } => (|| {
                failure.check().map_err(|e| e.field("failure"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("teardown_failed")),
            Self::TurnStarted { turn, tuning, via } => (|| {
                turn.check().map_err(|e| e.field("turn"))?;
                tuning.check().map_err(|e| e.field("tuning"))?;
                via.check().map_err(|e| e.field("via"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("turn_started")),
            Self::UsageDelta {
                usage,
                generation,
                provider,
            } => (|| {
                usage.check().map_err(|e| e.field("usage"))?;
                generation.check().map_err(|e| e.field("generation"))?;
                provider.check().map_err(|e| e.field("provider"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("usage_delta")),
            Self::Error { text } => (|| {
                text.check().map_err(|e| e.field("text"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("error")),
            Self::Nudge { cause, spent } => (|| {
                cause.check().map_err(|e| e.field("cause"))?;
                spent.check().map_err(|e| e.field("spent"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("nudge")),
            Self::ProviderError { error } => (|| {
                error.check().map_err(|e| e.field("error"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("provider_error")),
            Self::Retried { error } => (|| {
                error.check().map_err(|e| e.field("error"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("retried")),
            Self::SystemNote { text } => (|| {
                text.check().map_err(|e| e.field("text"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("system_note")),
            Self::HarnessResult { text } => (|| {
                text.check().map_err(|e| e.field("text"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("harness_result")),
            Self::Pin { key } => (|| {
                key.check().map_err(|e| e.field("key"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("pin")),
            Self::Unpin { key } => (|| {
                key.check().map_err(|e| e.field("key"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("unpin")),
            Self::Reap { title, cause } => (|| {
                title.check().map_err(|e| e.field("title"))?;
                cause.check().map_err(|e| e.field("cause"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("reap")),
            Self::Prune { names, idle_calls } => (|| {
                names.check().map_err(|e| e.field("names"))?;
                idle_calls.check().map_err(|e| e.field("idle_calls"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("prune")),
        })
    }
    fn hydrate(&self) -> Self::Value {
        on_json_stack(|| match self {
            Self::SessionEnded => Forensic::SessionEnded,
            Self::SessionClosed => Forensic::SessionClosed,
            Self::EngineSevered { cause } => Forensic::EngineSevered {
                cause: cause.hydrate(),
            },
            Self::TeardownFailed { failure } => Forensic::TeardownFailed {
                failure: failure.hydrate(),
            },
            Self::TurnStarted { turn, tuning, via } => Forensic::TurnStarted {
                turn: turn.hydrate(),
                tuning: tuning.hydrate(),
                via: via.hydrate(),
            },
            Self::UsageDelta {
                usage,
                generation,
                provider,
            } => Forensic::UsageDelta {
                usage: usage.hydrate(),
                generation: generation.hydrate(),
                provider: provider.hydrate(),
            },
            Self::Cancelled => Forensic::Cancelled,
            Self::Error { text } => Forensic::Error {
                text: text.hydrate(),
            },
            Self::Nudge { cause, spent } => Forensic::Nudge {
                cause: cause.hydrate(),
                spent: spent.hydrate(),
            },
            Self::ProviderError { error } => Forensic::ProviderError {
                error: error.hydrate(),
            },
            Self::Retried { error } => Forensic::Retried {
                error: error.hydrate(),
            },
            Self::SystemNote { text } => Forensic::SystemNote {
                text: text.hydrate(),
            },
            Self::HarnessResult { text } => Forensic::HarnessResult {
                text: text.hydrate(),
            },
            Self::Pin { key } => Forensic::Pin { key: key.hydrate() },
            Self::Unpin { key } => Forensic::Unpin { key: key.hydrate() },
            Self::Reap { title, cause } => Forensic::Reap {
                title: title.hydrate(),
                cause: cause.hydrate(),
            },
            Self::Prune { names, idle_calls } => Forensic::Prune {
                names: names.hydrate(),
                idle_calls: idle_calls.hydrate(),
            },
        })
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        on_json_stack(|| match value {
            Forensic::SessionEnded => Ok(Self::SessionEnded),
            Forensic::SessionClosed => Ok(Self::SessionClosed),
            Forensic::EngineSevered { cause } => (|| {
                Ok(Self::EngineSevered {
                    cause: <RawSevered as Mirror>::from_value(cause)
                        .map_err(|e| e.field("cause"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("engine_severed")),
            Forensic::TeardownFailed { failure } => (|| {
                Ok(Self::TeardownFailed {
                    failure: <String as Mirror>::from_value(failure)
                        .map_err(|e| e.field("failure"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("teardown_failed")),
            Forensic::TurnStarted { turn, tuning, via } => (|| {
                Ok(Self::TurnStarted {
                    turn: <u64 as Mirror>::from_value(turn).map_err(|e| e.field("turn"))?,
                    tuning: <Tuning as Mirror>::from_value(tuning)
                        .map_err(|e| e.field("tuning"))?,
                    via: <Option<RawVia> as Mirror>::from_value(via).map_err(|e| e.field("via"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("turn_started")),
            Forensic::UsageDelta {
                usage,
                generation,
                provider,
            } => (|| {
                Ok(Self::UsageDelta {
                    usage: <Usage as Mirror>::from_value(usage).map_err(|e| e.field("usage"))?,
                    generation: <Option<String> as Mirror>::from_value(generation)
                        .map_err(|e| e.field("generation"))?,
                    provider: <Option<String> as Mirror>::from_value(provider)
                        .map_err(|e| e.field("provider"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("usage_delta")),
            Forensic::Cancelled => Ok(Self::Cancelled),
            Forensic::Error { text } => (|| {
                Ok(Self::Error {
                    text: <String as Mirror>::from_value(text).map_err(|e| e.field("text"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("error")),
            Forensic::Nudge { cause, spent } => (|| {
                Ok(Self::Nudge {
                    cause: <String as Mirror>::from_value(cause).map_err(|e| e.field("cause"))?,
                    spent: <Option<Spent> as Mirror>::from_value(spent)
                        .map_err(|e| e.field("spent"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("nudge")),
            Forensic::ProviderError { error } => (|| {
                Ok(Self::ProviderError {
                    error: <RawProviderError as Mirror>::from_value(error)
                        .map_err(|e| e.field("error"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("provider_error")),
            Forensic::Retried { error } => (|| {
                Ok(Self::Retried {
                    error: <RawProviderError as Mirror>::from_value(error)
                        .map_err(|e| e.field("error"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("retried")),
            Forensic::SystemNote { text } => (|| {
                Ok(Self::SystemNote {
                    text: <String as Mirror>::from_value(text).map_err(|e| e.field("text"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("system_note")),
            Forensic::HarnessResult { text } => (|| {
                Ok(Self::HarnessResult {
                    text: <String as Mirror>::from_value(text).map_err(|e| e.field("text"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("harness_result")),
            Forensic::Pin { key } => (|| {
                Ok(Self::Pin {
                    key: <String as Mirror>::from_value(key).map_err(|e| e.field("key"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("pin")),
            Forensic::Unpin { key } => (|| {
                Ok(Self::Unpin {
                    key: <String as Mirror>::from_value(key).map_err(|e| e.field("key"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("unpin")),
            Forensic::Reap { title, cause } => (|| {
                Ok(Self::Reap {
                    title: <String as Mirror>::from_value(title).map_err(|e| e.field("title"))?,
                    cause: <String as Mirror>::from_value(cause).map_err(|e| e.field("cause"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("reap")),
            Forensic::Prune { names, idle_calls } => (|| {
                Ok(Self::Prune {
                    names: <Vec<String> as Mirror>::from_value(names)
                        .map_err(|e| e.field("names"))?,
                    idle_calls: <Vec<u64> as Mirror>::from_value(idle_calls)
                        .map_err(|e| e.field("idle_calls"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("prune")),
        })
    }
}
