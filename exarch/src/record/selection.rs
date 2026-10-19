//! The model in force, folded once off the forensic trail: resume and the view
//! fold both read it here rather than each keeping a copy.

use super::Forensic;

#[derive(Clone, Debug, Default)]
pub struct Selection {
    /// `(model, provider label)`, from the head bookend on.
    named: Option<(String, String)>,
    /// The window the catalog listed for the model in force.
    listed: Option<u64>,
    /// The tighter window an overflow taught for the model in force.
    narrowed: Option<u64>,
}

impl Selection {
    pub fn step(&mut self, forensic: &Forensic) {
        match forensic {
            Forensic::SessionStarted {
                model,
                label,
                context_window,
                ..
            }
            | Forensic::ModelChanged {
                model,
                label,
                context_window,
                ..
            } => {
                self.named = Some((model.clone(), label.clone()));
                self.listed = *context_window;
                self.narrowed = None;
            }
            Forensic::SessionResumed {
                model,
                label,
                context_window,
                ..
            } => {
                if self.model().is_none_or(|(named, _)| named != model) {
                    self.narrowed = None;
                }
                self.named = Some((model.clone(), label.clone()));
                self.listed = *context_window;
            }
            Forensic::WindowNarrowed { context_window } => self.narrowed = Some(*context_window),
            Forensic::SessionEnded
            | Forensic::SessionClosed
            | Forensic::TurnStarted { .. }
            | Forensic::UsageDelta { .. }
            | Forensic::Cancelled
            | Forensic::Error { .. }
            | Forensic::Nudge { .. }
            | Forensic::ProviderError { .. }
            | Forensic::Retried { .. }
            | Forensic::SystemNote { .. }
            | Forensic::HarnessResult { .. }
            | Forensic::Pin { .. }
            | Forensic::Unpin { .. }
            | Forensic::Reap { .. }
            | Forensic::Prune { .. } => {}
        }
    }

    /// `(model, provider)` in force — total from the session's first record,
    /// its head bookend, onward.
    pub fn model(&self) -> Option<(&str, &str)> {
        self.named.as_ref().map(|(m, p)| (m.as_str(), p.as_str()))
    }

    /// The context window the model in force has, as far as anything taught.
    pub fn window(&self) -> Option<u64> {
        self.narrowed.or(self.listed)
    }

    /// The `(model, window)` an overflow taught the live provider.
    pub fn learned(&self) -> Option<(String, u64)> {
        let (model, _) = self.named.as_ref()?;
        Some((model.clone(), self.narrowed?))
    }
}

#[cfg(test)]
mod tests;
