//! Coherent degradation: how far to trust a block, as two ordered signals.
//!
//! Context pressure is turn-level and every paragraph of a stressed turn
//! inherits it; echo similarity is per-block, catching prose that merely
//! restates the `ral` script just run.  [`super::md`] spends the two on
//! disjoint colour axes — a foreground saturation drain, a flat background
//! wash — never on value, which carries magnitude on the rail.

use std::collections::HashSet;

/// The two signals a [`super::block::Block`] carries.  `Default` is sound
/// prose — both `0`, no modulation — which is what chrome and cards get.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct Fidelity {
    /// Turn-level context-pressure floor, `0..=3`.
    pub(super) context: u8,
    /// Per-block echo delta, `0..=2`.
    pub(super) echo: u8,
}

/// Bucket `last_input` against the model's context window into a `0..=3`
/// floor.  A `None` window — an unlisted model, or a turn before the
/// catalog loads — reads as sound rather than as pressure.
pub(super) fn context_floor(last_input: u64, context_window: Option<u64>) -> u8 {
    match context_window {
        Some(cap) if cap > 0 => {
            #[allow(
                clippy::cast_precision_loss,
                reason = "token counts far below f64 precision limit (2^52)"
            )]
            let r = last_input as f64 / cap as f64;
            match r {
                r if r < 0.50 => 0,
                r if r < 0.75 => 1,
                r if r < 0.90 => 2,
                _ => 3,
            }
        }
        _ => 0,
    }
}

/// Bucket the trigram overlap of `prose` with the `script` it followed into a
/// `0..=2` delta: verbatim restatement is rubber-stamping, paraphrase is not.
pub(super) fn echo_delta(prose: &str, script: &str) -> u8 {
    match jaccard(cap(prose), cap(script)) {
        j if j < 0.20 => 0,
        j if j < 0.50 => 1,
        _ => 2,
    }
}

/// Word-trigram Jaccard similarity, `|a∩b| / |a∪b|`.  A text under three words
/// shingles to nothing; that counts as dissimilar, so a terse block never echoes.
fn jaccard(a: &str, b: &str) -> f32 {
    let wa: Vec<String> = a.split_whitespace().map(str::to_lowercase).collect();
    let wb: Vec<String> = b.split_whitespace().map(str::to_lowercase).collect();
    let (sa, sb) = (shingles(&wa), shingles(&wb));
    let inter = sa.intersection(&sb).count();
    let union = sa.len() + sb.len() - inter;
    if union == 0 {
        0.0
    } else {
        #[allow(
            clippy::cast_precision_loss,
            reason = "shingle-set sizes bounded by cap, far below f32 precision limit"
        )]
        let ratio = inter as f32 / union as f32;
        ratio
    }
}

/// The word-trigram shingles of `words`, which the caller has lowercased.
fn shingles(words: &[String]) -> HashSet<(&str, &str, &str)> {
    words
        .windows(3)
        .map(|w| (w[0].as_str(), w[1].as_str(), w[2].as_str()))
        .collect()
}

/// A representative 4 KB prefix of `s`, so a giant script can't stall a commit.
fn cap(s: &str) -> &str {
    const CAP: usize = 4096;
    if s.len() <= CAP {
        return s;
    }
    &s[..ral_core::text::floor_char_boundary(s, CAP)]
}
