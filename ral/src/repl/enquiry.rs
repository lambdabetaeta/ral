//! The REPL's two enquiry classes, typed once for both ends: the engine's
//! doors encode a request, the host decodes it, and back.
//!
//! - `` `repl-editor `` — the `_ed-*` doors' reads and writes of the editor
//!   context the host installs around a plugin hook's dispatch;
//! - `` `repl-plugin `` — the load doors telling the host a plugin's
//!   first-order manifest, which the host may refuse.

use ral_core::record;
use ral_core::serial::FOValue;
use ral_core::serial::datum::{Datum, tag, untag};

use super::plugin::manifest::Manifest;

const EDITOR: &str = "repl-editor";
const PLUGIN: &str = "repl-plugin";

/// One request to the host's editor context.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EditorOp {
    Get,
    Set {
        text: Option<String>,
        cursor: Option<usize>,
    },
    SetLbuffer(String),
    Insert(String),
    Push,
    Accept,
    Ghost(String),
    Highlight(Vec<HighlightReq>),
    History,
    StateGet,
    StateSet(Data),
}

/// A span as a plugin asked for it: `start`/`end` floored at zero, clamped by
/// the host against the text it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HighlightReq {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) style: String,
}

record!(HighlightReq {
    start: "start",
    end: "end",
    style: "style",
});

/// The `` `get `` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditorSnapshot {
    pub(crate) text: String,
    /// A character offset.
    pub(crate) cursor: usize,
    pub(crate) keymap: String,
    pub(crate) in_readline: bool,
}

record!(EditorSnapshot {
    text: "text",
    cursor: "cursor",
    keymap: "keymap",
    in_readline: "in-readline",
});

/// Any first-order value, as itself.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Data(pub(crate) FOValue);

impl Datum for Data {
    fn encode(self) -> FOValue {
        self.0
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        Ok(Self(v.clone()))
    }
}

/// What a load door tells the host.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PluginNote {
    Loaded(Manifest),
    Unloaded(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Enquiry {
    Editor(EditorOp),
    Plugin(PluginNote),
}

impl Datum for EditorOp {
    fn encode(self) -> FOValue {
        match self {
            Self::Get => tag("get", None),
            Self::Set { text, cursor } => tag(
                "set",
                Some(FOValue::Map {
                    entries: vec![
                        ("text".into(), text.encode()),
                        ("cursor".into(), cursor.encode()),
                    ],
                }),
            ),
            Self::SetLbuffer(s) => tag("set-lbuffer", Some(s.encode())),
            Self::Insert(s) => tag("insert", Some(s.encode())),
            Self::Push => tag("push", None),
            Self::Accept => tag("accept", None),
            Self::Ghost(s) => tag("ghost", Some(s.encode())),
            Self::Highlight(spans) => tag("highlight", Some(spans.encode())),
            Self::History => tag("history", None),
            Self::StateGet => tag("state-get", None),
            Self::StateSet(v) => tag("state-set", Some(v.encode())),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        use ral_core::serial::datum::{exact_keys, field};
        Ok(match untag(v) {
            Some(("get", None)) => Self::Get,
            Some(("set", Some(p))) => {
                exact_keys(p, &["text", "cursor"])?;
                Self::Set {
                    text: field(p, "text")?,
                    cursor: field(p, "cursor")?,
                }
            }
            Some(("set-lbuffer", Some(p))) => Self::SetLbuffer(Datum::decode(p)?),
            Some(("insert", Some(p))) => Self::Insert(Datum::decode(p)?),
            Some(("push", None)) => Self::Push,
            Some(("accept", None)) => Self::Accept,
            Some(("ghost", Some(p))) => Self::Ghost(Datum::decode(p)?),
            Some(("highlight", Some(p))) => Self::Highlight(Datum::decode(p)?),
            Some(("history", None)) => Self::History,
            Some(("state-get", None)) => Self::StateGet,
            Some(("state-set", Some(p))) => Self::StateSet(Datum::decode(p)?),
            _ => return Err(format!("no `{EDITOR} request is shaped like {}", v.shape())),
        })
    }
}

impl Datum for Enquiry {
    fn encode(self) -> FOValue {
        match self {
            Self::Editor(op) => tag(EDITOR, Some(op.encode())),
            Self::Plugin(PluginNote::Loaded(m)) => {
                tag(PLUGIN, Some(tag("loaded", Some(m.encode()))))
            }
            Self::Plugin(PluginNote::Unloaded(name)) => {
                tag(PLUGIN, Some(tag("unloaded", Some(name.encode()))))
            }
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some((EDITOR, Some(op))) => EditorOp::decode(op).map(Self::Editor),
            Some((PLUGIN, Some(note))) => match untag(note) {
                Some(("loaded", Some(m))) => {
                    Manifest::decode(m).map(|m| Self::Plugin(PluginNote::Loaded(m)))
                }
                Some(("unloaded", Some(n))) => {
                    String::decode(n).map(|n| Self::Plugin(PluginNote::Unloaded(n)))
                }
                _ => Err(format!(
                    "no `{PLUGIN} request is shaped like {} — expected `loaded or `unloaded",
                    note.shape()
                )),
            },
            Some((class, _)) => Err(format!(
                "the REPL answers no `{class} enquiries — only `{EDITOR} and `{PLUGIN}"
            )),
            None => Err(format!("an enquiry must be a variant, got {}", v.shape())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every request survives its own round trip.
    #[test]
    fn requests_round_trip() {
        for op in [
            EditorOp::Get,
            EditorOp::Set {
                text: Some("ls".into()),
                cursor: None,
            },
            EditorOp::Highlight(vec![HighlightReq {
                start: 0,
                end: 2,
                style: "command".into(),
            }]),
            EditorOp::StateSet(Data(FOValue::Int { value: 3 })),
        ] {
            let req = Enquiry::Editor(op);
            assert_eq!(Enquiry::decode(&req.clone().encode()), Ok(req));
        }
        let note = Enquiry::Plugin(PluginNote::Unloaded("p".into()));
        assert_eq!(Enquiry::decode(&note.clone().encode()), Ok(note));
    }

    /// An unknown class names itself, never passes silently.
    #[test]
    fn an_unknown_class_is_named() {
        let why = Enquiry::decode(&tag("exarch-agents", None)).expect_err("unknown class");
        assert!(why.contains("`exarch-agents"), "{why}");
    }
}
