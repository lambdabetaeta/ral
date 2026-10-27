//! Untrusted CONNECT bytes are decoded before any policy or DNS lookup.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = netgate::connect::decode(data);
});
