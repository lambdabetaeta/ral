use std::borrow::Cow;

/// `reason` is owned when it names the refused host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub reason: Cow<'static, str>,
}
