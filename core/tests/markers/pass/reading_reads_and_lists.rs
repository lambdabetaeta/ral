//! What a read judgment buys: bytes, metadata, a link's target, a listing.
//! Writing is judged to read too.

use ral_core::path::{Dir, Located, Reading, Writing};

fn read(located: &Located<Reading>) {
    let _ = located.read();
    let _ = located.stat();
    let _ = located.read_link();
}

fn read_what_is_written(located: &Located<Writing>) {
    let _ = located.read();
}

fn list(dir: &Dir<Reading>) {
    let _ = dir.read_dir();
}

fn main() {}
