//! The first-order vocabulary: data all the way down, knowing no `Value`.
//!
//! [`FOValue`] is first-order by construction over an extension slot that is
//! uninhabited by default ([`NoExt`]); [`datum`] types the protocol's
//! payloads through it.

mod bytes;
pub mod datum;
mod finite;

pub use bytes::Bytes;
pub use finite::Finite;

use crate::text::plural;
use serde::{Deserialize, Serialize};

/// A first-order ral value — data all the way down.  The extension slot `X`
/// is uninhabited by default, so a bare `FOValue` is first-order by
/// construction rather than by a checked invariant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FOValue<X = NoExt> {
    Unit,
    Bool {
        value: bool,
    },
    Int {
        value: i64,
    },
    Float {
        value: Finite,
    },
    String {
        value: std::string::String,
    },
    Bytes {
        value: Vec<u8>,
    },
    List {
        items: Vec<Self>,
    },
    Map {
        entries: Vec<(std::string::String, Self)>,
    },
    Variant {
        label: std::string::String,
        payload: Option<Box<Self>>,
    },
    Ext(X),
}

/// Uninhabited: a bare `FOValue` has no `Ext` arm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoExt {}

impl<X> FOValue<X> {
    /// The value's shape, named without quoting it.
    ///
    /// For a diagnostic that must say what arrived where something else was
    /// expected. A value that crossed a seam is foreign and unbounded, and the
    /// text goes on to be read by a model with a finite context, so quoting it
    /// would let the sender choose how much of that context to spend. Structure
    /// is what a shape error is about, so structure is all this renders.
    #[must_use]
    pub fn shape(&self) -> String {
        match self {
            Self::Unit => "()".to_string(),
            Self::Bool { .. } => "a Bool".to_string(),
            Self::Int { .. } => "an Int".to_string(),
            Self::Float { .. } => "a Float".to_string(),
            Self::String { .. } => "a Str".to_string(),
            Self::Bytes { value } => format!("{} bytes", value.len()),
            Self::List { items } => format!("a list of {}", plural(items.len(), "element")),
            Self::Map { entries } => format!("a record of {}", plural(entries.len(), "field")),
            // The label is the host's own alphabet, not caller-supplied text of
            // arbitrary size, so naming it costs nothing and is what a wrong tag
            // needs to hear.
            Self::Variant {
                label,
                payload: None,
            } => format!("the bare tag `{label}`"),
            Self::Variant {
                label,
                payload: Some(p),
            } => format!("`{label}` carrying {}", p.shape()),
            Self::Ext(_) => "a value that is not first-order".to_string(),
        }
    }

    /// A record's field; `None` on an absent key or a value that is no record.
    #[must_use]
    pub fn field(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Map { entries } => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String { value } => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int { value } => Some(*value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool { value } => Some(*value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes { value } => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_list(&self) -> Option<&[Self]> {
        match self {
            Self::List { items } => Some(items),
            _ => None,
        }
    }
}

impl FOValue {
    /// This value as JSON, `Bytes` through `bytes`, the one place JSON has no
    /// faithful form.  A variant is `{tag, payload}`.
    #[must_use]
    pub fn to_json(self, bytes: fn(&[u8]) -> serde_json::Value) -> serde_json::Value {
        use serde_json::Value as Json;
        match self {
            Self::Unit => Json::Null,
            Self::Bool { value } => Json::Bool(value),
            Self::Int { value } => value.into(),
            Self::Float { value } => value.get().into(),
            Self::String { value } => Json::String(value),
            Self::Bytes { value } => bytes(&value),
            Self::List { items } => {
                Json::Array(items.into_iter().map(|v| v.to_json(bytes)).collect())
            }
            Self::Map { entries } => Json::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, v.to_json(bytes)))
                    .collect(),
            ),
            Self::Variant { label, payload } => {
                let mut obj = serde_json::Map::new();
                obj.insert("tag".into(), Json::String(label));
                if let Some(p) = payload {
                    obj.insert("payload".into(), p.to_json(bytes));
                }
                Json::Object(obj)
            }
            Self::Ext(x) => match x {},
        }
    }
}

/// A leaf that is not data: what a seam scrubs to an `` `opaque `` placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opaque {
    Block,
    Function,
    Handle,
}

impl Opaque {
    /// `` `opaque [type: …] ``: what a scrubbed leaf crosses as, and what a
    /// fork holds where a handle stood.
    #[must_use]
    pub fn placeholder(self) -> FOValue {
        let kind = match self {
            Self::Block => "block",
            Self::Function => "function",
            Self::Handle => "handle",
        };
        datum::tag(
            OPAQUE_TAG,
            Some(FOValue::Map {
                entries: vec![("type".into(), FOValue::String { value: kind.into() })],
            }),
        )
    }
}

impl std::fmt::Display for Opaque {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Block => "a block",
            Self::Function => "a function",
            Self::Handle => "a handle",
        })
    }
}

/// Why a value has no [`FOValue`]: the first opaque leaf met, and whether it
/// sat inside a list, map, or variant rather than being the value itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotData {
    pub leaf: Opaque,
    pub nested: bool,
}

/// The label a placeholder carries — a `Variant`, never a bare string, so no
/// genuine string can impersonate one.
pub(crate) const OPAQUE_TAG: &str = "opaque";
