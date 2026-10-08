//! Per-vendor readers behind [`super::MeterSource`]. `LiveMeters::read`'s
//! `match` is the only place anything above this module switches on a
//! vendor.

pub(super) mod openrouter;
