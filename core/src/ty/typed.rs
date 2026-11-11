//! Types of data: the record and variant types ral shows for a Rust type, and
//! the [`Typed`] types that derive theirs from their own fields.

use super::{Label, Row, RowVar, Ty};
use crate::data::datum::Datum;
use crate::data::{Bytes, Finite};
use crate::record;
use crate::source::CallSite;
use std::collections::BTreeMap;
use std::num::NonZeroU8;
use std::sync::Arc;

/// A [`Datum`] whose encoding has a ral type.  Declared once, by the
/// `record!`, `variant!` or `label!` that derives the encoding.
pub trait Typed: Datum {
    fn ty() -> Ty;
}

/// The body of a [`Typed::ty`] that builds its type once: `$build` runs on the
/// first call, and every call is a clone of what it built.
#[macro_export]
macro_rules! memoized_ty {
    ($build:expr) => {{
        static TY: ::std::sync::LazyLock<$crate::ty::Ty> = ::std::sync::LazyLock::new(|| $build);
        TY.clone()
    }};
}

/// `T`'s type, found through a projection that is never run, so a record's
/// field types are never written beside the fields.
pub fn field_ty<S, T: Typed>(_: impl Fn(&S) -> &T) -> Ty {
    T::ty()
}

/// The row of `fields` under `label`, ending in `tail`.
fn rfold(fields: &[(&str, Ty)], tail: Row, label: fn(String) -> Label) -> Row {
    fields.iter().rev().fold(tail, |row, (name, ty)| {
        Row::extend(label((*name).to_string()), ty.clone(), row)
    })
}

/// A record type over a row of fields ending in `tail`.  The fields are
/// ordered by label, as the elaborator orders a literal's, so that a type
/// derived here is the type a literal of the same fields has.
pub fn record_row(fields: &[(&str, Ty)], tail: Row) -> Ty {
    let mut sorted = fields.to_vec();
    sorted.sort_by_key(|(name, _)| *name);
    Ty::record(rfold(&sorted, tail, Label::Field))
}

/// A record type over a closed row: the tail is `Empty`, so no extension.
pub fn closed_record(fields: &[(&str, Ty)]) -> Ty {
    record_row(fields, Row::empty())
}

/// A record type left open on `tail`: at least these fields, and any others.
pub fn open_record(fields: &[(&str, Ty)], tail: RowVar) -> Ty {
    record_row(fields, Row::var(tail))
}

/// A variant type over a row of tags with stated payloads, ending in `tail`.
pub fn variant_row(tags: &[(&str, Ty)], tail: Row) -> Ty {
    Ty::variant(rfold(tags, tail, Label::Case))
}

/// A variant type over a closed row of tags.
///
/// The tail is `Empty`, so a `case` on it must cover exactly these arms.  A
/// payload-less tag takes `Unit`, as `Inferencer::infer_val` gives one at its
/// construction site.
pub fn closed_variant(arms: &[(&str, Ty)]) -> Ty {
    variant_row(arms, Row::empty())
}

/// A variant left open on `tail`, so an unknown tag reaches the runtime door
/// that enumerates the legal ones rather than dying as a row-unification
/// mismatch.
pub fn open_variant(tags: &[(&str, Ty)], tail: RowVar) -> Ty {
    variant_row(tags, Row::var(tail))
}

/// The `` `some x | `none `` an optional field carries: an absent value is a
/// fact, not a missing key.
pub fn optional_ty(payload: Ty) -> Ty {
    closed_variant(&[("some", payload), ("none", Ty::unit())])
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
    () => Ty::unit(),
    u8 => Ty::int(),
    NonZeroU8 => Ty::int(),
    String => Ty::string(),
    Arc<str> => Ty::string(),
    bool => Ty::bool(),
    i32 => Ty::int(),
    i64 => Ty::int(),
    u32 => Ty::int(),
    u64 => Ty::int(),
    usize => Ty::int(),
    Finite => Ty::float(),
}

impl Typed for Bytes {
    fn ty() -> Ty {
        Ty::bytes()
    }
}

impl<T: Typed> Typed for Option<T> {
    fn ty() -> Ty {
        optional_ty(T::ty())
    }
}

impl<T: Typed> Typed for Box<T> {
    fn ty() -> Ty {
        T::ty()
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
