//! Tagged cards and settlements select payloads without serde Content replay.
use super::{
    Card, Count, CountRule, DecodeCause, DecodeFailure, Fields, JsonEncodeFailure, JsonRef,
    JsonShape, JsonShapeFailure, Mirror, RawChildFailure, RawHunk, Row, Serialize, Settlement,
    UnknownField, decode_structural, duplicate, length, missing, shape, unknown_tag,
};
use crate::card::{Field, Mark, Measure, Readout, Seg};
use ral_core::fact::DataStep;

// Only the subtrees pinned serde buffers as Content receive numeric coercion.
fn check_buffered_numbers(value: JsonRef<'_, '_>) -> Result<(), DecodeFailure> {
    enum Work<'d, 'a> {
        Visit(JsonRef<'d, 'a>),
        Push(DataStep),
        Pop,
    }
    let mut work = vec![Work::Visit(value)];
    let mut path = Vec::new();
    while let Some(next) = work.pop() {
        match next {
            Work::Push(step) => path.push(step),
            Work::Pop => {
                let _ = path.pop();
            }
            Work::Visit(value) => {
                if value.shape() == JsonShape::Number {
                    let _: serde_json::Number = decode_structural(value).map_err(|mut error| {
                        for step in path.iter().rev() {
                            error = error.at(step.clone());
                        }
                        error
                    })?;
                } else if let Some(items) = value.items() {
                    for (index, value) in items.enumerate().collect::<Vec<_>>().into_iter().rev() {
                        work.push(Work::Pop);
                        work.push(Work::Visit(value));
                        work.push(Work::Push(DataStep::Index(index.into())));
                    }
                } else if let Some(members) = value.members() {
                    for (key, value) in members.collect::<Vec<_>>().into_iter().rev() {
                        work.push(Work::Pop);
                        work.push(Work::Visit(value));
                        work.push(Work::Push(DataStep::Field(key.into())));
                    }
                }
            }
        }
    }
    Ok(())
}

fn internal<'d, 'a>(
    v: JsonRef<'d, 'a>,
    name: &str,
    offered: &[&str],
) -> Result<(&'d str, Fields<'d, 'a>), DecodeFailure> {
    if let Some(members) = v.members() {
        let mut tag = None;
        let mut fields = Vec::new();
        for (key, value) in members {
            if key == name {
                if tag.is_some() {
                    return Err(duplicate(name));
                }
                let text = value
                    .as_str()
                    .ok_or_else(|| shape(value, &[JsonShape::String]).field(name))?;
                if !offered.contains(&text) {
                    return Err(unknown_tag(text, offered).field(name));
                }
                tag = Some(text);
            } else {
                check_buffered_numbers(value).map_err(|e| e.field(key))?;
                fields.push((key, value));
            }
        }
        Ok((tag.ok_or_else(|| missing(name))?, Fields::Object(fields)))
    } else if let Some(mut items) = v.items() {
        let first = items.next().ok_or_else(|| missing(name))?;
        let tag = first
            .as_str()
            .ok_or_else(|| shape(first, &[JsonShape::String]).index(0u64.into()))?;
        if !offered.contains(&tag) {
            return Err(unknown_tag(tag, offered).index(0u64.into()));
        }
        let items = items.collect::<Vec<_>>();
        for (index, value) in items.iter().enumerate() {
            check_buffered_numbers(*value).map_err(|e| e.index((index + 1).into()))?;
        }
        Ok((tag, Fields::Sequence(items)))
    } else {
        Err(shape(v, &[JsonShape::Array, JsonShape::Object]))
    }
}

fn internal_unit(fields: Fields<'_, '_>) -> Result<(), DecodeFailure> {
    match fields {
        Fields::Sequence(items) if !items.is_empty() => {
            Err(length(CountRule::Exactly(0u64.into()), items.len()))
        }
        Fields::Object(_) | Fields::Sequence(_) => Ok(()),
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum RawSettlement {
    Replied,
    Failed { reason: RawChildFailure },
    Stopped,
}
impl Mirror for RawSettlement {
    type Value = Settlement;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let (tag, fields) = internal(v, "outcome", &["replied", "failed", "stopped"])?;
        match tag {
            // Pinned serde ignores object fields on an internal unit arm,
            // even with deny_unknown_fields; its sequence stays nullary.
            "replied" => internal_unit(fields).map(|()| Self::Replied).map_err(|e| e.payload(tag)),
            "stopped" => internal_unit(fields).map(|()| Self::Stopped).map_err(|e| e.payload(tag)),
            "failed" => (|| read_fields!(fields, true, Self::Failed, { reason: RawChildFailure => required }))()
                .map_err(|e: DecodeFailure| e.payload(tag)),
            _ => unreachable!("checked tag"),
        }
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        match self {
            Self::Failed { reason } => reason
                .check()
                .map_err(|e| e.field("reason").payload("failed")),
            Self::Replied | Self::Stopped => Ok(()),
        }
    }
    fn hydrate(&self) -> Settlement {
        match self {
            Self::Replied => Settlement::Replied,
            Self::Stopped => Settlement::Stopped,
            Self::Failed { reason } => Settlement::Failed {
                reason: reason.hydrate(),
            },
        }
    }
    fn from_value(v: &Settlement) -> Result<Self, JsonEncodeFailure> {
        Ok(match v {
            Settlement::Replied => Self::Replied,
            Settlement::Stopped => Self::Stopped,
            Settlement::Failed { reason } => Self::Failed {
                reason: RawChildFailure::from_value(reason)
                    .map_err(|e| e.field("reason").payload("failed"))?,
            },
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RawMeasure {
    label: String,
    #[serde(flatten)]
    readout: Readout,
}
impl RawMeasure {
    fn fields(fields: Fields<'_, '_>) -> Result<Self, DecodeFailure> {
        let Fields::Object(members) = fields else {
            return Err(DecodeFailure::new(DecodeCause::JsonShape(
                JsonShapeFailure {
                    expected: vec![JsonShape::Object],
                    found: JsonShape::Array,
                },
            )));
        };
        let mut label = None;
        let mut readout = Vec::new();
        for (key, value) in members {
            if key == "label" {
                if label.is_some() {
                    return Err(duplicate(key));
                }
                label = Some(String::read(value).map_err(|e| e.field(key))?);
            } else {
                check_buffered_numbers(value).map_err(|e| e.field(key))?;
                readout.push((key, value));
            }
        }
        let label = label.ok_or_else(|| missing("label"))?;
        let fields = Fields::Object(readout);
        let readout = read_fields!(fields, false, Readout, {
            value: u32 => required, max: Option<u32> => optional, unit: Option<String> => optional
        })?;
        Ok(Self { label, readout })
    }
}
impl Mirror for RawMeasure {
    type Value = Measure;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        Self::fields(Fields::object(v)?)
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        Ok(())
    }
    fn hydrate(&self) -> Measure {
        Measure {
            label: self.label.clone(),
            readout: self.readout.clone(),
        }
    }
    fn from_value(v: &Measure) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            label: v.label.clone(),
            readout: v.readout.clone(),
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "mark", rename_all = "snake_case")]
pub(crate) enum RawMark {
    Text { spans: Vec<crate::card::Span> },
    Measure(RawMeasure),
    Fields { rows: Vec<Field> },
    Diff { path: String, hunks: Vec<RawHunk> },
    Raw { bytes: Vec<u8> },
}
impl Mirror for RawMark {
    type Value = Mark;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let (tag, fields) = internal(v, "mark", &["text", "measure", "fields", "diff", "raw"])?;
        (|| match tag {
            "text" => read_fields!(fields, false, Self::Text, { spans: Vec<crate::card::Span> => required }),
            "measure" => RawMeasure::fields(fields).map(Self::Measure),
            "fields" => read_fields!(fields, false, Self::Fields, { rows: Vec<Field> => required }),
            "diff" => read_fields!(fields, false, Self::Diff, { path: String => required, hunks: Vec<RawHunk> => required }),
            "raw" => read_fields!(fields, false, Self::Raw, { bytes: Vec<u8> => required }),
            _ => unreachable!("checked tag"),
        })().map_err(|e: DecodeFailure| e.payload(tag))
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        match self {
            Self::Diff { hunks, .. } => hunks.check().map_err(|e| e.field("hunks").payload("diff")),
            Self::Text { .. } | Self::Measure(_) | Self::Fields { .. } | Self::Raw { .. } => Ok(()),
        }
    }
    fn hydrate(&self) -> Mark {
        match self {
            Self::Text { spans } => Mark::Text {
                spans: spans.clone(),
            },
            Self::Measure(v) => Mark::Measure(v.hydrate()),
            Self::Fields { rows } => Mark::Fields { rows: rows.clone() },
            Self::Diff { path, hunks } => Mark::Diff {
                path: path.clone(),
                hunks: hunks.hydrate(),
            },
            Self::Raw { bytes } => Mark::Raw {
                bytes: bytes.clone(),
            },
        }
    }
    fn from_value(v: &Mark) -> Result<Self, JsonEncodeFailure> {
        Ok(match v {
            Mark::Text { spans } => Self::Text {
                spans: spans.clone(),
            },
            Mark::Measure(v) => Self::Measure(RawMeasure::from_value(v)?),
            Mark::Fields { rows } => Self::Fields { rows: rows.clone() },
            Mark::Diff { path, hunks } => Self::Diff {
                path: path.clone(),
                hunks: <Vec<RawHunk> as Mirror>::from_value(hunks)
                    .map_err(|e| e.field("hunks").payload("diff"))?,
            },
            Mark::Raw { bytes } => Self::Raw {
                bytes: bytes.clone(),
            },
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct RawCard(Vec<RawMark>);
impl Mirror for RawCard {
    type Value = Card;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        Vec::read(v).map(Self)
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.0.check()
    }
    fn hydrate(&self) -> Card {
        Card(self.0.hydrate())
    }
    fn from_value(v: &Card) -> Result<Self, JsonEncodeFailure> {
        Vec::from_value(&v.0).map(Self)
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "tag", content = "segs", rename_all = "snake_case")]
pub(crate) enum RawRow {
    Context(Vec<Seg>),
    Del(Vec<Seg>),
    Add(Vec<Seg>),
}
impl RawRow {
    fn payload(tag: &str, v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let segs = Vec::<Seg>::read(v).map_err(|e| e.payload(tag))?;
        Ok(match tag {
            "context" => Self::Context(segs),
            "del" => Self::Del(segs),
            "add" => Self::Add(segs),
            _ => unreachable!("checked tag"),
        })
    }
    fn tag<'d>(v: JsonRef<'d, '_>) -> Result<&'d str, DecodeFailure> {
        let tag = v.as_str().ok_or_else(|| shape(v, &[JsonShape::String]))?;
        if ["context", "del", "add"].contains(&tag) {
            Ok(tag)
        } else {
            Err(unknown_tag(tag, &["context", "del", "add"]))
        }
    }
}
impl Mirror for RawRow {
    type Value = Row;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        if let Some(mut items) = v.items() {
            let got = items.len();
            let tag = Self::tag(
                items
                    .next()
                    .ok_or_else(|| length(CountRule::Exactly(2u64.into()), 0))?,
            )
            .map_err(|e| e.index(0u64.into()))?;
            let value = items
                .next()
                .ok_or_else(|| length(CountRule::Exactly(2u64.into()), 1))?;
            let row = Self::payload(tag, value).map_err(|e| e.index(1u64.into()))?;
            if got != 2 {
                return Err(length(CountRule::Exactly(2u64.into()), got));
            }
            return Ok(row);
        }
        let members = v
            .members()
            .ok_or_else(|| shape(v, &[JsonShape::Array, JsonShape::Object]))?;
        let mut tag = None;
        let mut content = None;
        let mut row = None;
        for (key, value) in members {
            match key {
                "tag" => {
                    if tag.is_some() {
                        return Err(duplicate(key));
                    }
                    tag = Some(Self::tag(value).map_err(|e| e.field(key))?);
                }
                "segs" => {
                    if content.is_some() {
                        return Err(duplicate(key));
                    }
                    if tag.is_none() {
                        check_buffered_numbers(value).map_err(|e| e.field(key))?;
                    }
                    content = Some(value);
                }
                _ => {}
            }
            if row.is_none()
                && let (Some(tag), Some(value)) = (tag, content)
            {
                row = Some(Self::payload(tag, value).map_err(|e| e.field("segs"))?);
            }
        }
        let _ = tag.ok_or_else(|| missing("tag"))?;
        row.ok_or_else(|| missing("segs"))
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        Ok(())
    }
    fn hydrate(&self) -> Row {
        match self {
            Self::Context(v) => Row::Context(v.clone()),
            Self::Del(v) => Row::Del(v.clone()),
            Self::Add(v) => Row::Add(v.clone()),
        }
    }
    fn from_value(v: &Row) -> Result<Self, JsonEncodeFailure> {
        Ok(match v {
            Row::Context(v) => Self::Context(v.clone()),
            Row::Del(v) => Self::Del(v.clone()),
            Row::Add(v) => Self::Add(v.clone()),
        })
    }
}
