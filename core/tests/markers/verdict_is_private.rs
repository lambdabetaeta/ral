//! An fs door's say-so is made in `authority/policy/fs.rs` alone, and the
//! module is private: no other crate can reach it, so none can judge a
//! handle itself.

use ral_core::authority::policy::fs::Verdict;

fn main() {}
