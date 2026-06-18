//! Coherent degradation: the epistemic signal a block carries.
//!
//! Move 7 of the "transcript as graphic" re-encoding
//! ([[decisions/260618_tui-transcript-as-graphic]]) extends the per-`Block`
//! variables past Bertin's planar set into the *epistemic* register: the
//! medium should carry *how much to trust* a passage, not just what it
//! says.  Two signals drive a per-block [`Fidelity`]:
//!
//! - **context pressure** (turn-level): `last_input` against the model's
//!   `context_window`, a floor every paragraph of a stressed turn inherits
//!   ([`context_floor`]);
//! - **echo similarity** (per-block): how closely the committing prose
//!   restates the `ral` script the model just ran — verbatim restatement
//!   is rubber-stamping, not synthesis ([`echo_delta`]).
//!
//! The renderer ([`super::md`]) turns these into a dimmer, lower-contrast
//! typeface (context) and a row-wise waver (echo), so a degraded answer no
//! longer borrows the visual authority of a sound one.

use std::collections::HashSet;

/// The two epistemic signals a [`super::block::Block`] carries, each a
/// small ordered level.  Default is *sound* — `context` and `echo` both
/// `0`, no modulation.  The two are kept separate rather than summed
/// because they drive two distinct media (value reduction vs waver), so a
/// single combined level could not reconstruct which treatment to apply.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct Fidelity {
    /// Turn-level context-pressure floor, `0..=3`: drives value reduction
    /// (dim + contrast pull).  `0` when the provider exposes no context
    /// window.
    pub(super) context: u8,
    /// Per-block echo delta, `0..=2`: trigram overlap of the committing
    /// prose with the most-recent `ral` script.  Drives the row waver.
    pub(super) echo: u8,
}

/// Bucket the turn's context pressure into a `0..=3` floor: `last_input`
/// against the model's `context_window`.  A `None` window (native
/// providers with no fetched catalog) yields a sound floor of `0` — no
/// signal, the renderer leaves the prose untouched.
pub(super) fn context_floor(last_input: u64, context_window: Option<u64>) -> u8 {
    match context_window {
        Some(cap) if cap > 0 => match last_input as f64 / cap as f64 {
            r if r < 0.50 => 0,
            r if r < 0.75 => 1,
            r if r < 0.90 => 2,
            _ => 3,
        },
        _ => 0,
    }
}

/// Bucket the echo similarity of `prose` against the `script` it followed
/// into a `0..=2` delta.  High verbatim overlap reads as the model
/// restating its own just-run script rather than synthesising, so it
/// degrades; a paraphrase lowers the trigram overlap and stays sound.
pub(super) fn echo_delta(prose: &str, script: &str) -> u8 {
    match jaccard(cap(prose), cap(script)) {
        j if j < 0.20 => 0,
        j if j < 0.50 => 1,
        _ => 2,
    }
}

/// Word-trigram Jaccard similarity of two texts: `|a∩b| / |a∪b|` over
/// their lowercased 3-gram shingle sets.  Cheap, dependency-free, and
/// bounded by the caller's [`cap`].  Two empty shingle sets (texts under
/// three words) are taken as dissimilar — `0.0` — so a terse call never
/// reads as an echo.
fn jaccard(a: &str, b: &str) -> f32 {
    let (sa, sb) = (shingles(a), shingles(b));
    let inter = sa.intersection(&sb).count();
    let union = sa.len() + sb.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f32 / union as f32
    }
}

/// The lowercased word-trigram shingles of `text`.
fn shingles(text: &str) -> HashSet<String> {
    let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
    words
        .windows(3)
        .map(|w| format!("{} {} {}", w[0], w[1], w[2]))
        .collect()
}

/// First 4 KB of `s`, truncated at a char boundary so a giant script
/// can't stall the commit — the comparison only needs a representative
/// prefix.
fn cap(s: &str) -> &str {
    const CAP: usize = 4096;
    if s.len() <= CAP {
        return s;
    }
    let mut end = CAP;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `context_floor` brackets the four pressure bands and floors a
    /// missing window to sound.
    #[test]
    fn context_floor_brackets_pressure() {
        let cap = Some(1000);
        assert_eq!(context_floor(400, cap), 0); // 40%
        assert_eq!(context_floor(600, cap), 1); // 60%
        assert_eq!(context_floor(800, cap), 2); // 80%
        assert_eq!(context_floor(950, cap), 3); // 95%
        assert_eq!(context_floor(950, None), 0, "no window → no signal");
    }

    /// Jaccard is `1.0` on identity, `0.0` on disjoint text, low on a
    /// paraphrase, and high on a verbatim restatement — the four points
    /// the echo bands are cut at.
    #[test]
    fn jaccard_separates_paraphrase_from_restatement() {
        let script = "read the parser module then run the failing test";
        assert_eq!(jaccard(script, script), 1.0, "identity is 1.0");
        assert_eq!(
            jaccard("alpha beta gamma delta", "one two three four five"),
            0.0,
            "disjoint is 0.0"
        );
        // A paraphrase shares words but few exact trigrams.
        let paraphrase = "I inspected the module and then executed the test that broke";
        assert!(
            jaccard(script, paraphrase) < 0.20,
            "paraphrase stays below the echo floor: {}",
            jaccard(script, paraphrase)
        );
        // A verbatim restatement (the script quoted back with a preamble)
        // overlaps heavily.
        let restatement = "I will read the parser module then run the failing test now";
        assert!(
            jaccard(script, restatement) > 0.50,
            "verbatim restatement clears the high band: {}",
            jaccard(script, restatement)
        );
    }

    /// `echo_delta` maps the Jaccard bands onto the `0..=2` levels.
    #[test]
    fn echo_delta_buckets_the_bands() {
        let script = "read the parser module then run the failing test";
        assert_eq!(echo_delta("entirely unrelated prose about cats", script), 0);
        assert_eq!(echo_delta(script, script), 2, "identity is the high band");
    }
}
