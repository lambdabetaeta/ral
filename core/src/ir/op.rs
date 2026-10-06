//! Binary primitives on values, by category.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// Arithmetic, ordering, equality: the category is the constructor, so a
/// handler matches exhaustively without a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinaryOp {
    Arith(ArithOp),
    Compare(CompareOp),
    Eq(EqOp),
}

/// Numeric, and may overflow. Division and modulo reject a zero divisor;
/// modulo also rejects floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// Numeric operands only; always a [`bool`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareOp {
    Lt,
    Gt,
    Le,
    Ge,
}

/// Structural on any value; always a [`bool`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EqOp {
    Eq,
    Ne,
}

impl CompareOp {
    pub fn holds(self, ordering: Ordering) -> bool {
        match self {
            Self::Lt => ordering.is_lt(),
            Self::Gt => ordering.is_gt(),
            Self::Le => ordering.is_le(),
            Self::Ge => ordering.is_ge(),
        }
    }
}

impl ArithOp {
    /// `None` on overflow, and on a zero divisor, which the caller names first.
    pub fn int(self, a: i64, b: i64) -> Option<i64> {
        match self {
            Self::Add => a.checked_add(b),
            Self::Sub => a.checked_sub(b),
            Self::Mul => a.checked_mul(b),
            Self::Div => a.checked_div(b),
            Self::Mod => a.checked_rem(b),
        }
    }

    /// `None` for `%`, which a float does not have.
    pub fn float(self, a: f64, b: f64) -> Option<f64> {
        match self {
            Self::Add => Some(a + b),
            Self::Sub => Some(a - b),
            Self::Mul => Some(a * b),
            Self::Div => Some(a / b),
            Self::Mod => None,
        }
    }
}
