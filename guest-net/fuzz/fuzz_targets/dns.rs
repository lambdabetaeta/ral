//! `cargo-fuzz` target for `guest_net::dns::Query::parse` — the crate's one
//! parser of bytes an adversary chose. Deliberately fuzzes `parse` alone,
//! not `answer`: everything `answer` touches beyond a successfully parsed
//! `Query` is either a fixed literal or an address `Names` already
//! validated, so `parse` returning `Some` is the only interesting boundary.
//!
//! Not run as part of this session's work (`cargo-fuzz` is not installed
//! here); `guest_net::dns`'s own `the_fuzz_corpus_never_panics_the_parser`
//! test replays `fuzz/corpus/dns/` under the normal test suite instead.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = guest_net::dns::Query::parse(data);
});
