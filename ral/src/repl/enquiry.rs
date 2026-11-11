//! The REPL's three enquiry classes, typed once for both ends: the engine's
//! doors encode a request, the host decodes it, and back.
//!
//! - `` `repl-editor `` — the `_ed-*` doors' reads and writes of the editor
//!   context the host installs around a plugin hook's dispatch;
//! - `` `repl-plugin `` — the load doors telling the host a plugin's
//!   first-order manifest, which the host may refuse;
//! - `` `repl-clipboard `` — `_ed-clipboard`'s copy, which the host performs.

use ral_core::authority::policy::{EditorVerb, Flag};
use ral_core::data::FOValue;
use ral_core::data::datum::{Datum, tag, untag};
use ral_core::fact::{
    DecodeCause, DecodeFailure, Expected, Found, ShapeFailure, UnexpectedPayload, UnknownTag,
};
use ral_core::record;

use super::plugin::Keymap;
use super::plugin::manifest::Manifest;

const EDITOR: &str = "repl-editor";
const PLUGIN: &str = "repl-plugin";
const CLIPBOARD: &str = "repl-clipboard";

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

/// A request an `_ed-*` door puts to the host, and the grant it needs.
pub(crate) trait Gated: Into<Enquiry> {
    fn flag(&self) -> Flag;
}

impl Gated for EditorOp {
    fn flag(&self) -> Flag {
        match self {
            Self::Get | Self::History | Self::StateGet => Flag::Editor(EditorVerb::Read),
            Self::Set { .. }
            | Self::SetLbuffer(_)
            | Self::Insert(_)
            | Self::Push
            | Self::Accept
            | Self::Ghost(_)
            | Self::Highlight(_)
            | Self::StateSet(_) => Flag::Editor(EditorVerb::Write),
        }
    }
}

impl Gated for ClipboardReq {
    fn flag(&self) -> Flag {
        Flag::Editor(EditorVerb::Write)
    }
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
    pub(crate) keymap: Keymap,
}

record!(typed EditorSnapshot {
    text: "text",
    cursor: "cursor",
    keymap: "keymap",
});

/// Any first-order value, as itself.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Data(pub(crate) FOValue);

impl Datum for Data {
    fn encode(self) -> FOValue {
        self.0
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        Ok(Self(v.clone()))
    }
}

/// A copy for the host to make; `osc52` is whether the terminal may be asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClipboardReq {
    pub(crate) text: String,
    pub(crate) osc52: bool,
}

record!(ClipboardReq {
    text: "text",
    osc52: "osc52",
});

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
    Clipboard(ClipboardReq),
}

impl From<EditorOp> for Enquiry {
    fn from(op: EditorOp) -> Self {
        Self::Editor(op)
    }
}

impl From<ClipboardReq> for Enquiry {
    fn from(req: ClipboardReq) -> Self {
        Self::Clipboard(req)
    }
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
            Self::StateSet(Data(value)) => tag("state-set", Some(value)),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        use ral_core::data::datum::{exact_keys, field};
        const TAGS: &[&str] = &[
            "get",
            "set",
            "set-lbuffer",
            "insert",
            "push",
            "accept",
            "ghost",
            "highlight",
            "history",
            "state-get",
            "state-set",
        ];
        let (label, payload) = tagged(v, TAGS)?;
        match label {
            "get" => bare(label, payload).map(|()| Self::Get),
            "set" => {
                let p = carried(label, payload)?;
                (|| {
                    exact_keys(p, &["text", "cursor"])?;
                    Ok(Self::Set {
                        text: field(p, "text")?,
                        cursor: field(p, "cursor")?,
                    })
                })()
                .map_err(|why: DecodeFailure| why.payload(label))
            }
            "set-lbuffer" => datum(label, payload).map(Self::SetLbuffer),
            "insert" => datum(label, payload).map(Self::Insert),
            "push" => bare(label, payload).map(|()| Self::Push),
            "accept" => bare(label, payload).map(|()| Self::Accept),
            "ghost" => datum(label, payload).map(Self::Ghost),
            "highlight" => datum(label, payload).map(Self::Highlight),
            "history" => bare(label, payload).map(|()| Self::History),
            "state-get" => bare(label, payload).map(|()| Self::StateGet),
            "state-set" => Ok(Self::StateSet(Data(
                payload.cloned().unwrap_or(FOValue::Unit),
            ))),
            _ => Err(unknown(label, TAGS)),
        }
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
            Self::Clipboard(req) => tag(CLIPBOARD, Some(req.encode())),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        const TAGS: &[&str] = &[EDITOR, PLUGIN, CLIPBOARD];
        let (label, payload) = tagged(v, TAGS)?;
        match label {
            EDITOR => datum(label, payload).map(Self::Editor),
            PLUGIN => {
                let note = carried(label, payload)?;
                (|| {
                    const NOTES: &[&str] = &["loaded", "unloaded"];
                    let (label, payload) = tagged(note, NOTES)?;
                    match label {
                        "loaded" => datum(label, payload).map(PluginNote::Loaded),
                        "unloaded" => datum(label, payload).map(PluginNote::Unloaded),
                        _ => Err(unknown(label, NOTES)),
                    }
                })()
                .map(Self::Plugin)
                .map_err(|why| why.payload(label))
            }
            CLIPBOARD => datum(label, payload).map(Self::Clipboard),
            _ => Err(unknown(label, TAGS)),
        }
    }
}

fn tagged<'a>(
    v: &'a FOValue,
    tags: &[&str],
) -> Result<(&'a str, Option<&'a FOValue>), DecodeFailure> {
    untag(v).ok_or_else(|| {
        DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
            expected: Expected::Tags(tags.iter().map(|tag| (*tag).into()).collect()),
            found: Found::of(v),
        }))
    })
}

fn unknown(tag: &str, tags: &[&str]) -> DecodeFailure {
    DecodeFailure::new(DecodeCause::UnknownTag(UnknownTag {
        tag: tag.into(),
        offered: tags.iter().map(|tag| (*tag).into()).collect(),
    }))
}

fn bare(tag: &str, payload: Option<&FOValue>) -> Result<(), DecodeFailure> {
    payload.map_or(Ok(()), |p| {
        Err(DecodeFailure::new(DecodeCause::UnexpectedPayload(
            UnexpectedPayload {
                tag: tag.into(),
                found: Found::of(p),
            },
        )))
    })
}

fn carried<'a>(tag: &str, payload: Option<&'a FOValue>) -> Result<&'a FOValue, DecodeFailure> {
    payload.ok_or_else(|| DecodeFailure::new(DecodeCause::MissingPayload(tag.into())))
}

fn datum<T: Datum>(tag: &str, payload: Option<&FOValue>) -> Result<T, DecodeFailure> {
    T::decode(carried(tag, payload)?).map_err(|why| why.payload(tag))
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
            EditorOp::StateSet(Data(FOValue::Unit)),
            EditorOp::StateSet(Data(FOValue::Int { value: 3 })),
        ] {
            let req = Enquiry::Editor(op);
            assert_eq!(Enquiry::decode(&req.clone().encode()), Ok(req));
        }
        let note = Enquiry::Plugin(PluginNote::Unloaded("p".into()));
        assert_eq!(Enquiry::decode(&note.clone().encode()), Ok(note));
        let copy = Enquiry::Clipboard(ClipboardReq {
            text: "hi".into(),
            osc52: true,
        });
        assert_eq!(Enquiry::decode(&copy.clone().encode()), Ok(copy));
    }

    #[test]
    fn an_absent_or_unit_state_payload_is_unit() {
        for payload in [None, Some(FOValue::Unit)] {
            let op = EditorOp::decode(&tag("state-set", payload)).expect("unit is valid state");
            assert_eq!(op, EditorOp::StateSet(Data(FOValue::Unit)));
            assert_eq!(op.encode(), tag("state-set", None));
        }
    }

    #[test]
    fn non_unit_editor_requests_still_require_their_payloads() {
        for label in ["set", "set-lbuffer", "insert", "ghost", "highlight"] {
            for payload in [None, Some(FOValue::Unit)] {
                assert!(EditorOp::decode(&tag(label, payload)).is_err(), "{label}");
            }
        }
    }
}
