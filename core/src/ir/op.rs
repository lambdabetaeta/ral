//! Binary primitives on values, by category.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;

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

/// The surface spelling, as written inside `$[…]`.
impl fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arith(op) => fmt::Display::fmt(op, f),
            Self::Compare(op) => fmt::Display::fmt(op, f),
            Self::Eq(op) => fmt::Display::fmt(op, f),
        }
    }
}

impl fmt::Display for ArithOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Mod => "%",
        })
    }
}

impl fmt::Display for CompareOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Lt => "<",
            Self::Gt => ">",
            Self::Le => "<=",
            Self::Ge => ">=",
        })
    }
}

impl fmt::Display for EqOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Eq => "==",
            Self::Ne => "!=",
        })
    }
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
