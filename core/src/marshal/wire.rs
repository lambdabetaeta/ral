//! The marshalled form: environment rows first, each naming only earlier rows;
//! a value names a row and holds no value.

use crate::ir::{Explicit, IrData, Name, Phase, Val};
use crate::shell::grant::RawSpawnGrant;
use crate::shell::runtime::{FrameKind, RawContext, Terms};
use crate::source::{SourceDb, Spanned};
use crate::ty::{Fixings, Scheme};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RawData {}

impl IrData for RawData {
    type Bytes = String;
    type Scheme = Arc<Scheme>;
    /// Pairs as written: any order, any repeat, until admission.
    type Fields<P: Phase> = Vec<(Name, Spanned<Val<P, Self>>)>;
}

pub(super) type RowId = u32;

#[derive(Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub(super) struct Wire<D: IrData = RawData> {
    pub rows: Vec<Row<D>>,
    pub env: Option<RowId>,
    pub context: RawContext<Vec<Frame<D>>>,
    pub sources: SourceDb,
    pub terms: Terms,
    pub fixings: Fixings,
    pub grant: RawSpawnGrant,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub(super) enum Row<D: IrData = RawData> {
    /// An environment: each name once.
    Env(Vec<(String, Binding<D>)>),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub(super) struct Binding<D: IrData = RawData> {
    pub value: Value<D>,
    pub scheme: Option<D::Scheme>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub(super) struct Frame<D: IrData = RawData> {
    pub entries: Vec<(String, Value<D>)>,
    pub catch_all: Option<Value<D>>,
    pub kind: FrameKind,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub(super) enum Value<D: IrData = RawData> {
    /// ⟨`tags` val, ρ⟩: the spine flat, `val` its foot; `None` is ∅.
    Closure {
        tags: Vec<String>,
        val: Val<Explicit, D>,
        env: Option<RowId>,
    },
    Builtin(String),
    /// η of `builtin` after `supplied` of its arguments, over env row `env`.
    Residual {
        builtin: String,
        env: RowId,
        supplied: u32,
    },
    /// A handle's title; it arrives withheld.
    Handle(String),
}
