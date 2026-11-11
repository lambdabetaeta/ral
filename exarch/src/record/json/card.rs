//! Complete serialization mirrors; domain checks precede semantic hydration.
use super::{
    Change, Count, CountRule, DecodeCause, DecodeFailure, Diff, Fields, Hunk, JsonEncodeFailure,
    JsonRef, Mirror, RawRow, Serialize, UnknownField, WriteOutcome, duplicate, length, missing,
};
#[derive(Debug, Serialize)]
pub(crate) struct RawChange {
    path: String,
    outcome: WriteOutcome,
    diff: Option<RawDiff>,
}
impl Mirror for RawChange {
    type Value = Change;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, false, Self, { path: String => required, outcome: WriteOutcome => required, diff: Option<RawDiff> => optional })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.path.check().map_err(|e| e.field("path"))?;
        self.outcome.check().map_err(|e| e.field("outcome"))?;
        self.diff.check().map_err(|e| e.field("diff"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        Change {
            path: self.path.hydrate(),
            outcome: self.outcome.hydrate(),
            diff: self.diff.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            path: <String as Mirror>::from_value(&value.path).map_err(|e| e.field("path"))?,
            outcome: <WriteOutcome as Mirror>::from_value(&value.outcome)
                .map_err(|e| e.field("outcome"))?,
            diff: <Option<RawDiff> as Mirror>::from_value(&value.diff)
                .map_err(|e| e.field("diff"))?,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RawDiff {
    hunks: Vec<RawHunk>,
    added: u32,
    removed: u32,
}
impl Mirror for RawDiff {
    type Value = Diff;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, false, Self, { hunks: Vec<RawHunk> => required, added: u32 => required, removed: u32 => required })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.hunks.check().map_err(|e| e.field("hunks"))?;
        self.added.check().map_err(|e| e.field("added"))?;
        self.removed.check().map_err(|e| e.field("removed"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        Diff {
            hunks: self.hunks.hydrate(),
            added: self.added.hydrate(),
            removed: self.removed.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            hunks: <Vec<RawHunk> as Mirror>::from_value(&value.hunks)
                .map_err(|e| e.field("hunks"))?,
            added: <u32 as Mirror>::from_value(&value.added).map_err(|e| e.field("added"))?,
            removed: <u32 as Mirror>::from_value(&value.removed).map_err(|e| e.field("removed"))?,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RawHunk {
    old: u32,
    new: u32,
    rows: Vec<RawRow>,
}
impl Mirror for RawHunk {
    type Value = Hunk;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, false, Self, { old: u32 => required, new: u32 => required, rows: Vec<RawRow> => required })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.old.check().map_err(|e| e.field("old"))?;
        self.new.check().map_err(|e| e.field("new"))?;
        self.rows.check().map_err(|e| e.field("rows"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        Hunk {
            old: self.old.hydrate(),
            new: self.new.hydrate(),
            rows: self.rows.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            old: <u32 as Mirror>::from_value(&value.old).map_err(|e| e.field("old"))?,
            new: <u32 as Mirror>::from_value(&value.new).map_err(|e| e.field("new"))?,
            rows: <Vec<RawRow> as Mirror>::from_value(&value.rows).map_err(|e| e.field("rows"))?,
        })
    }
}
