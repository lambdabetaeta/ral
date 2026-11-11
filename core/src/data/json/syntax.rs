//! Lossless syntax, with a flat destruction graph.

use crate::fact::{Count, JsonCategory, JsonShape, JsonSyntaxFailure};
use serde::de::{self, Visitor};
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::fmt;
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonId(usize);

#[derive(Debug)]
pub struct JsonDocument<'a> {
    input: &'a [u8],
    nodes: Vec<JsonNode<'a>>,
    root: JsonId,
}

#[derive(Debug, Clone, Copy)]
pub struct JsonRef<'d, 'a> {
    pub(super) document: &'d JsonDocument<'a>,
    pub(super) id: JsonId,
}

#[derive(Debug)]
pub(super) struct JsonNode<'a> {
    bytes: Range<usize>,
    pub(super) kind: JsonKind<'a>,
}

#[derive(Debug)]
pub(super) enum JsonKind<'a> {
    Null,
    Bool(bool),
    Number,
    String(Cow<'a, str>),
    Array(Vec<JsonId>),
    Object(Vec<JsonMember<'a>>),
}

#[derive(Debug)]
pub(super) struct JsonMember<'a> {
    pub(super) name: Cow<'a, str>,
    pub(super) name_bytes: Range<usize>,
    pub(super) value: JsonId,
}

struct Text;
impl<'a> Visitor<'a> for Text {
    type Value = Cow<'a, str>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON string")
    }
    fn visit_borrowed_str<E: de::Error>(self, value: &'a str) -> Result<Self::Value, E> {
        Ok(Cow::Borrowed(value))
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Cow::Owned(value.to_owned()))
    }
}

fn whitespace(input: &[u8], at: &mut usize) {
    while input.get(*at).is_some_and(u8::is_ascii_whitespace) {
        *at += 1;
    }
}

// RawValue checked delimiters and escapes; decoding also validates surrogates.
fn string<'a>(input: &'a [u8], at: &mut usize) -> Result<Cow<'a, str>, JsonSyntaxFailure> {
    let start = *at;
    *at += 1;
    loop {
        match input[*at] {
            b'"' => {
                *at += 1;
                break;
            }
            b'\\' => *at += 2,
            _ => *at += 1,
        }
    }
    let mut decoder = serde_json::Deserializer::from_slice(&input[start..*at]);
    serde::Deserializer::deserialize_str(&mut decoder, Text)
        .map_err(|error| syntax_failure(&error, input, start))
}

fn syntax_failure(error: &serde_json::Error, input: &[u8], offset: usize) -> JsonSyntaxFailure {
    let (preceding_lines, preceding_column) =
        input[..offset]
            .iter()
            .fold((0usize, 0usize), |(lines, column), &byte| {
                if byte == b'\n' {
                    (lines + 1, 0)
                } else {
                    (lines, column + 1)
                }
            });
    JsonSyntaxFailure {
        class: match error.classify() {
            serde_json::error::Category::Syntax => JsonCategory::Syntax,
            serde_json::error::Category::Eof => JsonCategory::Eof,
            serde_json::error::Category::Data | serde_json::error::Category::Io => {
                unreachable!("syntax parsing from bytes has no semantic or I/O failure")
            }
        },
        line: Count::from(preceding_lines + error.line()),
        column: Count::from(
            error.column()
                + if error.line() == 1 {
                    preceding_column
                } else {
                    0
                },
        ),
        detail: error.to_string(),
    }
}

impl<'a> JsonDocument<'a> {
    /// # Errors
    /// Returns the JSON syntax failure before constructing the syntax arena.
    ///
    /// # Panics
    /// Only if the arena walk contradicts the preceding syntax validation.
    pub fn parse(input: &'a [u8]) -> Result<Self, JsonSyntaxFailure> {
        let _: &RawValue =
            serde_json::from_slice(input).map_err(|error| syntax_failure(&error, input, 0))?;
        let mut nodes: Vec<JsonNode<'a>> = Vec::new();
        let mut parents: Vec<JsonId> = Vec::new();
        let mut at = 0;
        loop {
            whitespace(input, &mut at);
            match input.get(at) {
                None => break,
                Some(b',' | b':') => {
                    at += 1;
                    continue;
                }
                Some(b']' | b'}') => {
                    at += 1;
                    let id = parents.pop().expect("validated closing delimiter");
                    nodes[id.0].bytes.end = at;
                    continue;
                }
                _ => {}
            }
            let name = if parents
                .last()
                .is_some_and(|id| matches!(nodes[id.0].kind, JsonKind::Object(_)))
            {
                let start = at;
                let name = string(input, &mut at)?;
                let bytes = start..at;
                whitespace(input, &mut at);
                at += 1; // colon
                whitespace(input, &mut at);
                Some((name, bytes))
            } else {
                None
            };
            let start = at;
            let kind = match input[at] {
                b'[' => {
                    at += 1;
                    JsonKind::Array(Vec::new())
                }
                b'{' => {
                    at += 1;
                    JsonKind::Object(Vec::new())
                }
                b'"' => JsonKind::String(string(input, &mut at)?),
                b'n' => {
                    at += 4;
                    JsonKind::Null
                }
                b't' => {
                    at += 4;
                    JsonKind::Bool(true)
                }
                b'f' => {
                    at += 5;
                    JsonKind::Bool(false)
                }
                _ => {
                    while input.get(at).is_some_and(|b| {
                        !b.is_ascii_whitespace() && !matches!(b, b',' | b']' | b'}')
                    }) {
                        at += 1;
                    }
                    JsonKind::Number
                }
            };
            let container = matches!(kind, JsonKind::Array(_) | JsonKind::Object(_));
            let id = JsonId(nodes.len());
            nodes.push(JsonNode {
                bytes: start..at,
                kind,
            });
            if let Some(parent) = parents.last() {
                match &mut nodes[parent.0].kind {
                    JsonKind::Array(items) => items.push(id),
                    JsonKind::Object(members) => {
                        let (name, name_bytes) = name.expect("object member has a name");
                        members.push(JsonMember {
                            name,
                            name_bytes,
                            value: id,
                        });
                    }
                    _ => unreachable!("only containers enter the parent stack"),
                }
            }
            if container {
                parents.push(id);
            }
        }
        Ok(Self {
            input,
            nodes,
            root: JsonId(0),
        })
    }

    pub fn root(&self) -> JsonRef<'_, 'a> {
        JsonRef {
            document: self,
            id: self.root,
        }
    }
}

impl<'d, 'a> JsonRef<'d, 'a> {
    pub(super) fn node(self) -> &'d JsonNode<'a> {
        &self.document.nodes[self.id.0]
    }

    pub(super) fn member_source(self, member: &JsonMember<'a>) -> &'a [u8] {
        &self.document.input[member.name_bytes.clone()]
    }

    pub(super) fn child(self, id: JsonId) -> Self {
        Self {
            document: self.document,
            id,
        }
    }

    pub fn shape(self) -> JsonShape {
        match self.node().kind {
            JsonKind::Null => JsonShape::Null,
            JsonKind::Bool(_) => JsonShape::Bool,
            JsonKind::Number => JsonShape::Number,
            JsonKind::String(_) => JsonShape::String,
            JsonKind::Array(_) => JsonShape::Array,
            JsonKind::Object(_) => JsonShape::Object,
        }
    }

    pub fn byte_range(self) -> Range<usize> {
        self.node().bytes.clone()
    }

    pub fn source(self) -> &'a [u8] {
        &self.document.input[self.byte_range()]
    }

    /// # Panics
    /// Only if a syntax-validated number token is not UTF-8.
    pub fn number(self) -> Option<&'a str> {
        matches!(self.node().kind, JsonKind::Number)
            .then(|| std::str::from_utf8(self.source()).expect("validated number"))
    }

    pub fn as_str(self) -> Option<&'d str> {
        match &self.node().kind {
            JsonKind::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_bool(self) -> Option<bool> {
        match self.node().kind {
            JsonKind::Bool(value) => Some(value),
            _ => None,
        }
    }

    pub fn items(self) -> Option<impl ExactSizeIterator<Item = Self> + 'd> {
        match &self.node().kind {
            JsonKind::Array(items) => Some(items.iter().map(move |id| self.child(*id))),
            _ => None,
        }
    }

    pub fn members(self) -> Option<impl ExactSizeIterator<Item = (&'d str, Self)> + 'd> {
        match &self.node().kind {
            JsonKind::Object(members) => Some(
                members
                    .iter()
                    .map(move |m| (m.name.as_ref(), self.child(m.value))),
            ),
            _ => None,
        }
    }
}
