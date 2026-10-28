#![deny(clippy::wildcard_enum_match_arm)]

use exarch::card::{Card, Field, Hunk, Mark, Measure, Span};
use exarch::clock;
use exarch::provider::accounts::{self, Entry};
use exarch::record::AgentState;
use exarch::record::fault::{Datum, Readout, Remedy};
use ral_core::path::{basename, extension};
use serde::Serialize;
use std::path::PathBuf;
use ts_rs::TS;

use crate::folder::changes::{Change, JobReport};

#[derive(Serialize, Clone, Copy, PartialEq, Eq, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    None,
    SignedIn,
    Keyless,
    Vault,
    /// Environment keys outrank saved keys.
    Environment,
}

impl From<accounts::Source> for Source {
    fn from(source: accounts::Source) -> Self {
        match source {
            accounts::Source::None => Self::None,
            accounts::Source::SignedIn => Self::SignedIn,
            accounts::Source::Keyless => Self::Keyless,
            accounts::Source::Vault => Self::Vault,
            accounts::Source::Environment => Self::Environment,
        }
    }
}

#[derive(Serialize, Clone, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct Account {
    /// Commands use this id; labels are display-only and need not be unique.
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) source: Source,
    /// At most the last four key characters.
    pub(crate) hint: Option<String>,
    pub(crate) shadowed: Option<String>,
    pub(crate) env_var: Option<String>,
    pub(crate) endpoint: Option<String>,
    pub(crate) protocol: Option<String>,
    pub(crate) withdrawable: bool,
}

impl From<Entry> for Account {
    fn from(entry: Entry) -> Self {
        Self {
            id: entry.id,
            label: entry.label,
            source: entry.source.into(),
            hint: entry.hint,
            shadowed: entry.shadowed,
            env_var: entry.env_var,
            endpoint: entry.endpoint,
            protocol: entry.protocol,
            withdrawable: entry.withdrawable,
        }
    }
}

#[derive(Serialize, Clone, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct AccountList {
    pub(crate) accounts: Vec<Account>,
    pub(crate) vault: String,
    pub(crate) protocols: Vec<String>,
}

/// Unknown model capabilities admit reasoning; gray out only on positive absence.
#[derive(Serialize, Clone, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct ModelChoice {
    pub(crate) name: String,
    pub(crate) reasoning: bool,
}

#[derive(Serialize, Clone, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Models {
    Loading,
    Listed { choices: Vec<ModelChoice> },
    Failed { reason: String },
}

#[derive(Serialize, Clone, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct ProviderChoice {
    /// Round-trips through Choice by account id, never by display label.
    pub(crate) account: String,
    pub(crate) label: String,
    pub(crate) models: Models,
}

#[derive(Serialize, Clone, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct ModelMenu {
    pub(crate) providers: Vec<ProviderChoice>,
    pub(crate) efforts: Vec<String>,
    pub(crate) default_effort: String,
}

#[derive(Clone, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct Opening {
    pub(crate) label: String,
    pub(crate) model: String,
    /// The effort actually in force after capability masking.
    pub(crate) effort: String,
}

/// Absent effort keeps the thinking-on default; explicit "auto" sends no reasoning option.
#[derive(serde::Deserialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct Choice {
    pub(crate) account: String,
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
}

#[derive(Clone, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct SignInStep {
    pub(crate) say: String,
    pub(crate) link: Option<String>,
}

/// Open records preserve readable payloads and a checked tag; Measure uses unknown to keep its tag.
#[derive(Clone, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
#[serde(tag = "mark", rename_all = "snake_case")]
pub(crate) enum MarkDto {
    Text {
        #[ts(type = "Record<string, any>[]")]
        spans: Vec<Span>,
    },
    Measure(#[ts(type = "Record<string, unknown>")] Measure),
    Fields {
        #[ts(type = "Record<string, any>[]")]
        rows: Vec<Field>,
    },
    Diff {
        path: String,
        #[ts(type = "Record<string, any>[]")]
        hunks: Vec<Hunk>,
    },
    Raw {
        text: String,
    },
}

pub(crate) fn marks_dto(card: Card) -> Vec<MarkDto> {
    card.0
        .into_iter()
        .map(|mark| match mark {
            Mark::Text { spans } => MarkDto::Text { spans },
            Mark::Measure(m) => MarkDto::Measure(m),
            Mark::Fields { rows } => MarkDto::Fields { rows },
            Mark::Diff { path, hunks } => MarkDto::Diff { path, hunks },
            Mark::Raw { bytes } => MarkDto::Raw {
                text: String::from_utf8_lossy(&bytes).into_owned(),
            },
        })
        .collect()
}

#[derive(Clone, Copy, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
#[serde(rename_all = "snake_case")]
pub(crate) enum Severity {
    Warn,
    Bad,
}

#[derive(Clone, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct ReadoutDto {
    pub(crate) headline: String,
    pub(crate) fields: Vec<RowDto>,
    pub(crate) hints: Vec<String>,
    pub(crate) severity: Severity,
}

#[derive(Clone, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct RowDto {
    pub(crate) label: String,
    pub(crate) value: String,
}

impl ReadoutDto {
    pub(crate) fn new(r: Readout, severity: Severity) -> Self {
        Self {
            headline: r.headline,
            fields: r
                .fields
                .into_iter()
                .map(|f| RowDto {
                    label: f.label,
                    value: match f.datum {
                        Datum::Text(text) => text,
                        Datum::Seconds(secs) => clock::hms(secs),
                    },
                })
                .collect(),
            hints: r.remedies.into_iter().filter_map(hint).collect(),
            severity,
        }
    }
}

fn hint(remedy: Remedy) -> Option<String> {
    match remedy {
        Remedy::Reroute => Some("start again with a model that has a larger window"),
        Remedy::SignIn => Some("sign in again under Accounts, then start again"),
        Remedy::Evict | Remedy::RaiseMaxTokens => None,
    }
    .map(str::to_string)
}

#[derive(Clone, Copy, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentStateDto {
    Ready,
    AwaitingModel,
    Evaluating,
    Evicting,
    WaitingOnAgents,
}

impl From<AgentState> for AgentStateDto {
    fn from(state: AgentState) -> Self {
        match state {
            AgentState::Ready => Self::Ready,
            AgentState::AwaitingModel => Self::AwaitingModel,
            AgentState::Evaluating => Self::Evaluating,
            AgentState::Evicting => Self::Evicting,
            AgentState::WaitingOnAgents => Self::WaitingOnAgents,
        }
    }
}

/// What the window does before any agent exists to have a state.
#[derive(Clone, Copy, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Preparation {
    FinishingPrevious,
    Starting,
    ReadingFolder {
        #[ts(type = "number")]
        files: u64,
    },
}

#[derive(Clone, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum SynodEvent {
    /// What the user said, drawn where the trunk took it up.
    Human {
        text: String,
    },
    Token {
        text: String,
    },
    /// Closes the last markdown block without waiting for a following one.
    Boundary,
    /// The status bar holds this state until replaced.
    State {
        state: AgentStateDto,
    },
    /// Pre-agent progress; the next state or preparation replaces it.
    Preparing {
        stage: Preparation,
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
    Card {
        marks: Vec<MarkDto>,
    },
    /// Process cards belong in the dial, never the transcript.
    ProcessCard {
        marks: Vec<MarkDto>,
    },
    /// Busy helpers counted by identity; emitted only when the count changes.
    Helpers {
        busy: u32,
    },
    HelperDone {
        name: String,
        ok: bool,
        elapsed_secs: f64,
    },
    Usage {
        input: u64,
        output: u64,
        dollars: f64,
        unmetered: bool,
    },
    StopReason {
        reason: String,
    },
    Error {
        message: String,
    },
    ProviderError(ReadoutDto),
    /// Always Warn: the exchange survived and its partial reply stands.
    Stalled(ReadoutDto),
    /// Emitted by the window, never the bus; opening failure ends, exchange failure permits retry.
    Failure {
        message: String,
    },
}

#[derive(Clone, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct ConversationEvent {
    pub(crate) attempt: String,
    pub(crate) update: ConversationUpdate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct ExchangeId(pub(crate) u32);

#[derive(Clone, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) enum ConversationUpdate {
    Opening {
        opening: Opening,
    },
    Event {
        event: SynodEvent,
    },
    ExchangeBegan {
        exchange: ExchangeId,
    },
    /// The prompts still waiting, oldest first; replaces the previous snapshot.
    Queue {
        messages: Vec<String>,
    },
    ExchangeDone {
        exchange: ExchangeId,
        stopped: bool,
        /// None means the walk failed, never an empty report.
        report: Option<WindowReport>,
    },
    Ended {
        /// Whether a failure event already explained the end.
        explained: bool,
    },
}

#[derive(Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct PickedFolder {
    /// Native path returned to `start_conversation` unchanged, never shown.
    pub(crate) path: PathBuf,
    pub(crate) label: String,
}

/// Offsets are UTF-16 code units for JavaScript slicing, never lexer byte offsets.
#[derive(Clone, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct RalToken {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) class: String,
}

#[derive(Clone, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum SignInDone {
    /// The refreshed menu precedes completion; label is display-only.
    SignedIn {
        label: String,
        replaced: bool,
    },
    Failed {
        reason: String,
    },
}

#[derive(Clone, Copy, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChangeKind {
    Created,
    Modified,
    /// Timestamp-only evidence cannot prove changed contents.
    Touched,
    Deleted,
    Renamed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
#[serde(rename_all = "snake_case")]
pub(crate) enum OpenAs {
    Open,
    Reveal,
}

#[derive(Clone, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct ChangeFile {
    /// Relative current name, including after a rename; the window's row key.
    pub(crate) path: String,
    pub(crate) rename_from: Option<String>,
    pub(crate) kind: ChangeKind,
    /// Present only for a surviving plain file; the window sends its index and shown name.
    pub(crate) open: Option<OpenAs>,
}

#[derive(Clone, Serialize, TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub(crate) struct WindowReport {
    pub(crate) files: Vec<ChangeFile>,
    pub(crate) unreadable: Vec<String>,
}

/// HTML is excluded: a browser runs scripts and can reach other host files.
const OPENABLE: &[&str] = &[
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "txt", "md",
    "csv", "tsv", "json", "png", "jpg", "jpeg", "gif", "webp", "svg",
];

/// Judge the final extension; refuse colons on every host because Windows treats them as streams.
pub(crate) fn open_as(rel: &str) -> OpenAs {
    let openable = !basename(rel).contains(':')
        && extension(rel).is_some_and(|ext| OPENABLE.iter().any(|ok| ok.eq_ignore_ascii_case(ext)));
    if openable {
        OpenAs::Open
    } else {
        OpenAs::Reveal
    }
}

impl WindowReport {
    pub(crate) fn of(report: &JobReport) -> Self {
        let files = report
            .changes
            .iter()
            .map(|change| {
                let (kind, shown, from) = match change {
                    Change::Created { path, .. } => (ChangeKind::Created, path.clone(), None),
                    Change::Modified { path, .. } => (ChangeKind::Modified, path.clone(), None),
                    Change::Touched { path } => (ChangeKind::Touched, path.clone(), None),
                    Change::Deleted { path, .. } => (ChangeKind::Deleted, path.clone(), None),
                    Change::Renamed { from, to } => {
                        (ChangeKind::Renamed, to.clone(), Some(from.clone()))
                    }
                };
                ChangeFile {
                    open: change.leaves_file().then(|| open_as(&shown)),
                    rename_from: from,
                    path: shown,
                    kind,
                }
            })
            .collect();

        Self {
            files,
            unreadable: report.unreadable.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_opens_only_where_a_plain_file_now_stands() {
        use crate::folder::manifest::EntryKind;

        let created = |path: &str, now| Change::Created {
            path: path.into(),
            now,
        };
        let report = JobReport {
            changes: vec![
                created(
                    "file.pdf",
                    EntryKind::File {
                        size: 1,
                        mtime_ns: 1,
                        mode: 0o644,
                    },
                ),
                created(
                    "link.pdf",
                    EntryKind::Link {
                        target: "elsewhere".into(),
                    },
                ),
                Change::Modified {
                    path: "became-a-folder.pdf".into(),
                    now: EntryKind::Folder,
                },
                Change::Deleted {
                    path: "gone.pdf".into(),
                    folder: false,
                },
            ],
            unreadable: Vec::new(),
        };
        let opens: Vec<_> = WindowReport::of(&report)
            .files
            .iter()
            .map(|file| file.open)
            .collect();
        assert_eq!(opens, [Some(OpenAs::Open), None, None, None]);
    }

    #[test]
    fn an_unavailable_report_is_null() {
        let update = ConversationUpdate::ExchangeDone {
            exchange: ExchangeId(1),
            stopped: false,
            report: None,
        };
        assert_eq!(
            serde_json::to_string(&update).expect("serializable update"),
            r#"{"type":"exchange_done","exchange":1,"stopped":false,"report":null}"#,
        );
    }

    #[test]
    fn an_available_report_is_an_object() {
        let update = ConversationUpdate::ExchangeDone {
            exchange: ExchangeId(2),
            stopped: true,
            report: Some(WindowReport {
                files: vec![ChangeFile {
                    path: "report.pdf".into(),
                    rename_from: None,
                    kind: ChangeKind::Created,
                    open: Some(OpenAs::Open),
                }],
                unreadable: vec!["private".into()],
            }),
        };
        assert_eq!(
            serde_json::to_value(&update).expect("serializable update"),
            serde_json::json!({
                "type": "exchange_done",
                "exchange": 2,
                "stopped": true,
                "report": {
                    "files": [{
                        "path": "report.pdf",
                        "rename_from": null,
                        "kind": "created",
                        "open": "open",
                    }],
                    "unreadable": ["private"],
                },
            }),
        );
    }

    #[test]
    fn documents_open_and_everything_else_is_shown_in_its_folder() {
        for name in [
            "report.pdf",
            "notes/Minutes.DOCX",
            "a/b/chart.Png",
            "data.csv",
            "README.md",
            "diagram.svg",
        ] {
            assert_eq!(open_as(name), OpenAs::Open, "{name}");
        }
        for name in [
            "summary.bat",
            "Invoice.pdf.lnk",
            "report.url",
            "page.html",
            "page.HTM",
            "setup.exe",
            "script.ps1",
            "Makefile",
            "trailing.pdf.",
            "spaced.pdf ",
            "tool.exe:notes.txt",
            "sub.pdf/inner.cmd",
        ] {
            assert_eq!(open_as(name), OpenAs::Reveal, "{name}");
        }
    }
}
