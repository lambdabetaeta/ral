// The typed-site vocabulary of synod's binary: see `lint/site.rs`.
include!("../../../lint/site.rs");

/// Sites that raise no card.
#[derive(Clone, Copy)]
pub(crate) enum Silent {
    /// Marks a file the guest wrote as having come from the internet: the
    /// `Zone.Identifier` stream beside it, set by the shell before the user
    /// opens it; the file's own bytes are untouched.  It raises no card: it is
    /// the shell's own mark, not a write the model made.
    MarkOfTheWeb,
}

impl Site for Silent {}
