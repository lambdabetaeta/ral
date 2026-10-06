//! Types of data: the record and variant types ral shows for a Rust type, and
//! the [`Typed`] types that derive theirs from their own fields.

use super::{Label, Row, RowVar, Ty};
use crate::first_order::Bytes;
use crate::first_order::datum::Datum;
use crate::record;
use crate::source::CallSite;
use std::collections::BTreeMap;
use std::sync::Arc;

/// A [`Datum`] whose encoding has a ral type.  Declared once, by the
/// `record!`, `variant!` or `label!` that derives the encoding.
pub trait Typed: Datum {
    fn ty() -> Ty;
}

/// `T`'s type, found through a projection that is never run, so a record's
/// field types are never written beside the fields.
pub fn field_ty<S, T: Typed>(_: impl Fn(&S) -> &T) -> Ty {
    T::ty()
}

/// The row of `fields` under `label`, ending in `tail`.
fn rfold(fields: &[(&str, Ty)], tail: Row, label: fn(String) -> Label) -> Row {
    fields.iter().rev().fold(tail, |row, (name, ty)| {
        Row::Extend(
            label((*name).to_string()),
            Box::new(ty.clone()),
            Box::new(row),
        )
    })
}

/// A record type over a row of fields ending in `tail`.
pub fn record_row(fields: &[(&str, Ty)], tail: Row) -> Ty {
    Ty::Record(rfold(fields, tail, Label::Field))
}

/// A record type over a closed row: the tail is `Empty`, so no extension.
pub fn closed_record(fields: &[(&str, Ty)]) -> Ty {
    record_row(fields, Row::Empty)
}

/// A record type left open on `tail`: at least these fields, and any others.
pub fn open_record(fields: &[(&str, Ty)], tail: RowVar) -> Ty {
    record_row(fields, Row::Var(tail))
}

/// A variant type over a row of tags with stated payloads, ending in `tail`.
pub fn variant_row(tags: &[(&str, Ty)], tail: Row) -> Ty {
    Ty::Variant(rfold(tags, tail, Label::Case))
}

/// A variant type over a closed row of tags.
///
/// The tail is `Empty`, so a `case` on it must cover exactly these arms.  A
/// payload-less tag takes `Unit`, as `Inferencer::infer_val` gives one at its
/// construction site.
pub fn closed_variant(arms: &[(&str, Ty)]) -> Ty {
    variant_row(arms, Row::Empty)
}

/// A variant left open on `tail`, so an unknown tag reaches the runtime door
/// that enumerates the legal ones rather than dying as a row-unification
/// mismatch.
pub fn open_variant(tags: &[(&str, Ty)], tail: RowVar) -> Ty {
    variant_row(tags, Row::Var(tail))
}

/// The `` `some x | `none `` an optional field carries: an absent value is a
/// fact, not a missing key.
pub fn optional_ty(payload: Ty) -> Ty {
    closed_variant(&[("some", payload), ("none", Ty::Unit)])
}

macro_rules! leaf {
    ($($t:ty => $ty:expr),* $(,)?) => {
        $(impl Typed for $t {
            fn ty() -> Ty {
                $ty
            }
        })*
    };
}

leaf! {
    String => Ty::String,
    Arc<str> => Ty::String,
    bool => Ty::Bool,
    i32 => Ty::Int,
    i64 => Ty::Int,
    u32 => Ty::Int,
    u64 => Ty::Int,
    usize => Ty::Int,
}

impl Typed for Bytes {
    fn ty() -> Ty {
        Ty::Bytes
    }
}

impl<T: Typed> Typed for Option<T> {
    fn ty() -> Ty {
        optional_ty(T::ty())
    }
}

impl<T: Typed> Typed for Vec<T> {
    fn ty() -> Ty {
        Ty::list(T::ty())
    }
}

impl<T: Typed> Typed for BTreeMap<String, T> {
    fn ty() -> Ty {
        Ty::map(T::ty())
    }
}

// Here, not in `source`: `Typed` is this module's, and `source` sits below it.
record!(typed CallSite {
    script: "script",
    line: "line",
    col: "col",
});
