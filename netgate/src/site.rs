include!("../../lint/site.rs");

#[derive(Clone, Copy)]
pub(crate) enum Silent {
    /// Resolution is ledgered with the verdict; the invoking exec supplies the card.
    VetResolve,
    /// Vetted dials are ledgered; the invoking exec supplies the card.
    VetDial,
}

impl Site for Silent {}
