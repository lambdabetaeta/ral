//! Complete serialization mirrors; domain checks precede semantic hydration.
use super::{
    AgentId, Count, CountRule, DecodeCause, DecodeFailure, EditAuthority, Eviction, Fields,
    ItemKind, JsonEncodeFailure, JsonRef, Linked, Location, Locus, Mirror, Pointer, Range, RawSpan,
    RawSum, Role, Seq, Serialize, SpanRow, TurnKind, TurnRow, UnknownField, duplicate, external,
    length, missing, on_json_stack, required_payload, unit,
};
#[derive(Debug, Serialize)]
pub(crate) struct RawLocus {
    seq: Seq,
    bytes: Range<u64>,
    sum: RawSum,
}
impl Mirror for RawLocus {
    type Value = Locus;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, false, Self, { seq: Seq => required, bytes: Range<u64> => required, sum: RawSum => required })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.seq.check().map_err(|e| e.field("seq"))?;
        self.bytes.check().map_err(|e| e.field("bytes"))?;
        self.sum.check().map_err(|e| e.field("sum"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        Locus {
            seq: self.seq.hydrate(),
            bytes: self.bytes.hydrate(),
            sum: self.sum.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            seq: <Seq as Mirror>::from_value(&value.seq).map_err(|e| e.field("seq"))?,
            bytes: <Range<u64> as Mirror>::from_value(&value.bytes)
                .map_err(|e| e.field("bytes"))?,
            sum: <RawSum as Mirror>::from_value(&value.sum).map_err(|e| e.field("sum"))?,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RawEviction {
    span: RawSpan,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    by: EditAuthority,
}
impl Mirror for RawEviction {
    type Value = Eviction;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, true, Self, { span: RawSpan => required, note: Option<String> => optional, by: EditAuthority => required })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.span.check().map_err(|e| e.field("span"))?;
        self.note.check().map_err(|e| e.field("note"))?;
        self.by.check().map_err(|e| e.field("by"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        Eviction {
            span: self.span.hydrate(),
            note: self.note.hydrate(),
            by: self.by.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            span: <RawSpan as Mirror>::from_value(&value.span).map_err(|e| e.field("span"))?,
            note: <Option<String> as Mirror>::from_value(&value.note)
                .map_err(|e| e.field("note"))?,
            by: <EditAuthority as Mirror>::from_value(&value.by).map_err(|e| e.field("by"))?,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RawPointer {
    source: AgentId,
    loci: Vec<RawLocus>,
}
impl Mirror for RawPointer {
    type Value = Pointer;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, false, Self, { source: AgentId => required, loci: Vec<RawLocus> => required })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.source.check().map_err(|e| e.field("source"))?;
        self.loci.check().map_err(|e| e.field("loci"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        Pointer {
            source: self.source.hydrate(),
            loci: self.loci.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            source: <AgentId as Mirror>::from_value(&value.source)
                .map_err(|e| e.field("source"))?,
            loci: <Vec<RawLocus> as Mirror>::from_value(&value.loci)
                .map_err(|e| e.field("loci"))?,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RawSpanRow {
    span: RawSpan,
    role: Role,
    kind: ItemKind,
    label: String,
    bytes: usize,
    location: RawLocation,
}
impl Mirror for RawSpanRow {
    type Value = SpanRow;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, true, Self, { span: RawSpan => required, role: Role => required, kind: ItemKind => required, label: String => required, bytes: usize => required, location: RawLocation => required })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.span.check().map_err(|e| e.field("span"))?;
        self.role.check().map_err(|e| e.field("role"))?;
        self.kind.check().map_err(|e| e.field("kind"))?;
        self.label.check().map_err(|e| e.field("label"))?;
        self.bytes.check().map_err(|e| e.field("bytes"))?;
        self.location.check().map_err(|e| e.field("location"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        SpanRow {
            span: self.span.hydrate(),
            role: self.role.hydrate(),
            kind: self.kind.hydrate(),
            label: self.label.hydrate(),
            bytes: self.bytes.hydrate(),
            location: self.location.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            span: <RawSpan as Mirror>::from_value(&value.span).map_err(|e| e.field("span"))?,
            role: <Role as Mirror>::from_value(&value.role).map_err(|e| e.field("role"))?,
            kind: <ItemKind as Mirror>::from_value(&value.kind).map_err(|e| e.field("kind"))?,
            label: <String as Mirror>::from_value(&value.label).map_err(|e| e.field("label"))?,
            bytes: <usize as Mirror>::from_value(&value.bytes).map_err(|e| e.field("bytes"))?,
            location: <RawLocation as Mirror>::from_value(&value.location)
                .map_err(|e| e.field("location"))?,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RawTurnRow {
    id: u64,
    role: Role,
    kind: TurnKind,
    label: String,
    bytes: usize,
    location: RawLocation,
}
impl Mirror for RawTurnRow {
    type Value = TurnRow;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, true, Self, { id: u64 => required, role: Role => required, kind: TurnKind => required, label: String => required, bytes: usize => required, location: RawLocation => required })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.id.check().map_err(|e| e.field("id"))?;
        self.role.check().map_err(|e| e.field("role"))?;
        self.kind.check().map_err(|e| e.field("kind"))?;
        self.label.check().map_err(|e| e.field("label"))?;
        self.bytes.check().map_err(|e| e.field("bytes"))?;
        self.location.check().map_err(|e| e.field("location"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        TurnRow {
            id: self.id.hydrate(),
            role: self.role.hydrate(),
            kind: self.kind.hydrate(),
            label: self.label.hydrate(),
            bytes: self.bytes.hydrate(),
            location: self.location.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            id: <u64 as Mirror>::from_value(&value.id).map_err(|e| e.field("id"))?,
            role: <Role as Mirror>::from_value(&value.role).map_err(|e| e.field("role"))?,
            kind: <TurnKind as Mirror>::from_value(&value.kind).map_err(|e| e.field("kind"))?,
            label: <String as Mirror>::from_value(&value.label).map_err(|e| e.field("label"))?,
            bytes: <usize as Mirror>::from_value(&value.bytes).map_err(|e| e.field("bytes"))?,
            location: <RawLocation as Mirror>::from_value(&value.location)
                .map_err(|e| e.field("location"))?,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RawLinked {
    row: RawTurnRow,
    at: RawPointer,
}
impl Mirror for RawLinked {
    type Value = Linked;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, true, Self, { row: RawTurnRow => required, at: RawPointer => required })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.row.check().map_err(|e| e.field("row"))?;
        self.at.check().map_err(|e| e.field("at"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        Linked {
            row: self.row.hydrate(),
            at: self.at.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            row: <RawTurnRow as Mirror>::from_value(&value.row).map_err(|e| e.field("row"))?,
            at: <RawPointer as Mirror>::from_value(&value.at).map_err(|e| e.field("at"))?,
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RawLocation {
    Resident,
    Evicted(RawSpan),
    Withdrawn,
}
impl Mirror for RawLocation {
    type Value = Location;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        on_json_stack(|| {
            let (tag, payload) = external(value, &["resident", "evicted", "withdrawn"])?;
            match tag {
                "resident" => unit(payload, tag).map(|()| Self::Resident),
                "evicted" => <RawSpan>::read(required_payload(payload, tag)?)
                    .map(Self::Evicted)
                    .map_err(|e| e.payload(tag)),
                "withdrawn" => unit(payload, tag).map(|()| Self::Withdrawn),
                _ => unreachable!("external checked tag"),
            }
        })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        on_json_stack(|| match self {
            Self::Resident | Self::Withdrawn => Ok(()),
            Self::Evicted(value) => value.check().map_err(|e| e.payload("evicted")),
        })
    }
    fn hydrate(&self) -> Self::Value {
        on_json_stack(|| match self {
            Self::Resident => Location::Resident,
            Self::Evicted(value) => Location::Evicted(value.hydrate()),
            Self::Withdrawn => Location::Withdrawn,
        })
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        on_json_stack(|| match value {
            Location::Resident => Ok(Self::Resident),
            Location::Evicted(value) => <RawSpan as Mirror>::from_value(value)
                .map(Self::Evicted)
                .map_err(|e| e.payload("evicted")),
            Location::Withdrawn => Ok(Self::Withdrawn),
        })
    }
}
