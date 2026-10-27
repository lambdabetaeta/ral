include!("../../../lint/site.rs");

#[derive(Clone, Copy)]
pub(crate) enum Silent {
    /// Reads the build's own media manifest before packaging; no guest grant applies.
    MediaManifest,
    /// Probes the build's own boot media to distinguish an absent image from an unchecked one.
    MediaProbe,
}

impl Site for Silent {}
