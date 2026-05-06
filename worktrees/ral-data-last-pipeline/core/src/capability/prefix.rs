//! Path prefix utilities for capability checks.
//!
//! `GrantPath` holds both the lexical and canonical forms of a grant prefix,
//! making the path duality explicit rather than recomputed ad hoc.
//! `canonical_grant_paths` builds them from a policy's raw string list.
//! `intersect_grant_paths` keeps the deeper prefix from each overlapping
//! pair — the meet of two prefix-set policies.

use crate::types::Dynamic;

/// A grant prefix in both its canonical (symlink-resolved) and raw (lexical) forms.
///
/// Canonical form is used internally during the meet-fold so two layers
/// that name the same directory through different symlinks (e.g. `/tmp`
/// and `/private/tmp` on macOS) are recognised as overlapping.  Raw form
/// is what survives into the projection emitted to Seatbelt / bwrap, so
/// the OS sandbox sees the lexical path the profile author wrote.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct GrantPath {
    pub canonical: String,
    pub raw: String,
}

/// Build `GrantPath`s from a policy's raw prefix list.
///
/// Each entry is sigil-expanded once (`~`, `xdg:NAME[/sub]`), then
/// resolved two ways: canonically (symlinks followed) and lexically
/// (`.`/`..` normalised, kept as-written for the OS sandbox profile).
/// Without the shared expansion the lexical form would carry literal
/// `~/.cache` or `xdg:config` strings into Seatbelt, which match
/// nothing.  Result is sorted and deduped.
pub(super) fn canonical_grant_paths(dynamic: &Dynamic, prefixes: &[String]) -> Vec<GrantPath> {
    let resolver = dynamic.resolver();
    let mut out: Vec<GrantPath> = prefixes
        .iter()
        .map(|prefix| {
            let lex = resolver.lex(prefix);
            let canonical = crate::path::canon::canonicalise_lenient(&lex)
                .to_string_lossy()
                .into_owned();
            let raw = lex.to_string_lossy().into_owned();
            GrantPath { canonical, raw }
        })
        .collect();
    out.sort_by(|a, b| a.canonical.cmp(&b.canonical).then_with(|| a.raw.cmp(&b.raw)));
    out.dedup();
    out
}

/// Extract the raw strings from a slice of `GrantPath`s (sorted, deduped).
pub(super) fn grant_path_raws(prefixes: &[GrantPath]) -> Vec<String> {
    crate::types::unique_strings(prefixes.iter().map(|p| p.raw.clone()))
}

/// Intersect two grant-path sets.
///
/// A path is admitted by the intersection iff some prefix in `left`
/// and some prefix in `right` both cover it.  For each covering pair,
/// the deeper prefix survives.
///
/// Symlink-aware: overlap is decided on the canonical form, so two
/// layers that name the same directory via different symlinks
/// (`/tmp` vs `/private/tmp`) intersect correctly.  This is the
/// runtime counterpart to the lexical
/// `crate::types::capability::intersect_prefix_strings` used by
/// `Capabilities::meet`, which has no `Dynamic` and therefore
/// cannot canonicalise.
pub(super) fn intersect_grant_paths(left: &[GrantPath], right: &[GrantPath]) -> Vec<GrantPath> {
    use std::path::Path;
    let mut out = Vec::new();
    for a in left {
        for b in right {
            let a_path = Path::new(&a.canonical);
            let b_path = Path::new(&b.canonical);
            if a.canonical == b.canonical {
                out.push(a.clone());
                out.push(b.clone());
            } else if crate::path::path_within(a_path, b_path) {
                out.push(a.clone());
            } else if crate::path::path_within(b_path, a_path) {
                out.push(b.clone());
            }
        }
    }
    out.sort_by(|a, b| a.canonical.cmp(&b.canonical).then_with(|| a.raw.cmp(&b.raw)));
    out.dedup();
    out
}
