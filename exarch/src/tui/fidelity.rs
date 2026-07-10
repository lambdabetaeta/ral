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
//! The renderer ([`super::md`]) turns these into a foreground saturation
//! drain (context) and a flat background wash (echo), so a degraded answer
//! no longer borrows the visual authority of a sound one — and neither
//! treatment touches the value (lightness) channel that carries magnitude.

use std::collections::HashSet;

/// The two epistemic signals a [`super::block::Block`] carries, each a
/// small ordered level.  Default is *sound* — `context` and `echo` both
/// `0`, no modulation.  The two are kept separate rather than summed
/// because they drive two disjoint colour axes (foreground drain vs
/// background wash), so a single combined level could not reconstruct
/// which treatment to apply.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct Fidelity {
    /// Turn-level context-pressure floor, `0..=3`: drains the foreground's
    /// saturation toward grey at held luminance.  `0` when the provider
    /// exposes no context window.
    pub(super) context: u8,
    /// Per-block echo delta, `0..=2`: trigram overlap of the committing
    /// prose with the most-recent `ral` script.  Shades the field behind
    /// the prose with a flat wash.
    pub(super) echo: u8,
}

/// Bucket the turn's context pressure into a `0..=3` floor: `last_input`
/// against the model's `context_window`.  A `None` window (native
/// providers with no fetched catalog) yields a sound floor of `0` — no
/// signal, the renderer leaves the prose untouched.
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
        #[allow(
            clippy::cast_precision_loss,
            reason = "shingle-set sizes bounded by cap, far below f32 precision limit"
        )]
        let ratio = inter as f32 / union as f32;
        ratio
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
