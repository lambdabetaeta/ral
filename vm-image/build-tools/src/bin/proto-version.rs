//! Print the protocol version this source speaks, and nothing else.
//!
//! `vm-image/build-boot.sh` records it in the boot media's manifest, and the
//! recording must not be able to lag the engine it describes — so the script
//! compiles the constant out of the same checkout that becomes that engine
//! rather than grepping for it. Sibling of the `boot-contract` build tool.
//!
//! One line on stdout, no label: a shell reads it with `$(…)`.

#![allow(
    clippy::disallowed_macros,
    reason = "[silent:build-tool] the packaging script reads this result"
)]
fn main() {
    println!("{}", ral_core::protocol::PROTOCOL_VERSION);
}
