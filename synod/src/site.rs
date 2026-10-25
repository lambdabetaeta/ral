// The typed-site vocabulary of synod's library: see `lint/site.rs`.
include!("../../lint/site.rs");

/// Sites that raise no card.
#[derive(Clone, Copy)]
pub(crate) enum Silent {
    /// Reads the launching user's real home folder from this process's own
    /// environment: the territory a grant is protected against is a fact of the
    /// host, so no overlay applies.  It raises no card: the folders it names are
    /// walked and judged afterwards.
    HostTerritory,
    /// Lists the folder the user picked to check readability before a grant
    /// exists.  It raises no card: this tests the user's own access, not a
    /// model request under the grant being opened.
    GrantReadability,
    /// Lists the user's workspace and reads link targets without following
    /// them to record a manifest.  It raises no card: this is synod's own
    /// change bookkeeping, not the model's I/O; no file bytes are read.
    WorkspaceManifest,
    /// A file of the boot media this build ships, looked for and read by name
    /// in the program's own install tree (a checkout's image pipeline output,
    /// in a development build): the kernel, the initramfs, the rootfs and its
    /// checksum.  It raises no card: they are the program's own installed
    /// files, which no grant judges.
    InstallTree,
}

impl Site for Silent {}
