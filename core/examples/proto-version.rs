//! Print the protocol version this source speaks, and nothing else.
//!
//! `vm-image/build-boot.sh` records it in the boot media's manifest, and the
//! recording must not be able to lag the engine it describes — so the script
//! compiles the constant out of the same checkout that becomes that engine
//! rather than grepping for it. Sibling of `ral-daemon`'s `boot-contract`.
//!
//! One line on stdout, no label: a shell reads it with `$(…)`.

fn main() {
    println!("{}", ral_core::protocol::PROTOCOL_VERSION);
}
