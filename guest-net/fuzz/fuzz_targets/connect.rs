//! `cargo-fuzz` target for `guest_net::connect::decode` — the crate's one
//! parser of bytes the guest tool chose, ahead of any policy or DNS lookup.
//!
//! Not run as part of this session's work (`cargo-fuzz` is not installed
//! here); `guest_net::connect`'s own `the_fuzz_corpus_never_panics_the_parser`
//! test replays `fuzz/corpus/connect/` under the normal test suite instead.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = guest_net::connect::decode(data);
});
