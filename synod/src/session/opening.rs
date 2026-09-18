//! What the window shows before the first message: who is answering — see
//! [`Opening`].

/// What the window shows before the first message: who is answering, and
/// at what effort.
#[derive(Clone, serde::Serialize, ts_rs::TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub struct Opening {
    /// The answering account's
    /// [`identity::label`](exarch::provider::identity::label), set-relative
    /// to every account available when the conversation began. A display
    /// string, and named as one on the wire, unlike
    /// [`crate::session::Choice::account`]'s id.
    pub label: String,
    /// The model that account is driving.
    pub model: String,
    /// The [`EFFORT_LADDER`](exarch::provider::EFFORT_LADDER) label of the
    /// effort actually in force, after
    /// [`crate::session::resolve_tuning`]'s masking — not what was asked
    /// for, which the window already knows and which a model that takes no
    /// reasoning control never receives.
    pub effort: String,
}
