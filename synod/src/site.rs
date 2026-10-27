include!("../../lint/site.rs");

#[derive(Clone, Copy)]
pub(crate) enum Silent {
    /// Real host environment, without an overlay; territory is walked and judged afterwards.
    HostTerritory,
    /// The user's readability check before a grant exists.
    GrantReadability,
    /// Host change bookkeeping: listings and link targets only, never file bytes.
    FolderManifest,
    /// Host provenance mark before opening a guest-written file; its bytes stay untouched.
    #[cfg(windows)]
    MarkOfTheWeb,
    /// The user's report button: no-follow walk and kind check, without reading file bytes.
    #[cfg(any(target_os = "macos", windows))]
    ReviewTarget,
}

impl Site for Silent {}
