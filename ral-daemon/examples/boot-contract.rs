//! Print the boot contract version this source speaks, and nothing else.
//!
//! `vm-image/build-boot.sh` has to record `boot::CONTRACT` in the boot media's
//! manifest, and the one thing that recording must never be is stale — a
//! number copied by hand, or grepped out of a source line whose spelling has
//! since moved, would record precisely the drift it exists to catch.  So the
//! script does not read the constant: it *compiles* it, from the same checkout
//! and in the same cargo invocation that produces the `ral-daemon` going into
//! the initramfs, and asks the result.  A number that disagreed with the
//! shipped daemon could then only come from a compiler that disagreed with
//! itself.
//!
//! One line on stdout, no label and no newline of ceremony, because a shell
//! reads it with `$(…)` and nothing else ever will.

fn main() {
    println!("{}", ral_daemon::boot::CONTRACT);
}
