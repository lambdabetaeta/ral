//! Per-vendor readers behind [`super::MeterSource`]. `LiveMeters::read`'s
//! `match` is the only place anything above these two modules switches on a
//! vendor.

pub(super) mod codex;
pub(super) mod openrouter;
