//! The grant stack's say-so is made in `capability/fs.rs` alone, and the
//! module is private: no other crate can reach it, so none can judge a
//! handle itself.

use ral_core::capability::fs::Verdict;

fn main() {}
