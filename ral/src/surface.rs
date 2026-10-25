//! Surfaced values both front-ends print themselves: a watched worker's
//! lines, and the note a value of no class they know is dropped with.

use ral_core::data::FOValue;
use ral_core::data::datum::untag;
use ral_core::shell::runtime::{Observation, WatchEvent};

/// `'watch {label, line}`'s printed line, if `v` is one.
pub(crate) fn watch_line(v: &FOValue) -> Option<String> {
    WatchEvent::from_surface(v).map(|w| format!("[{}] {}", w.label, w.line))
}

/// The note `v` is dropped with; none for an observation, which neither
/// front-end renders by choice.
pub(crate) fn dropped(v: &FOValue) -> Option<String> {
    let class = match untag(v) {
        Some((Observation::SURFACE_TAG, _)) => return None,
        Some((label, _)) => format!("`{label}"),
        None => v.shape(),
    };
    Some(format!(
        "note: dropped a surfaced value ({class}): this front-end renders no such value"
    ))
}
