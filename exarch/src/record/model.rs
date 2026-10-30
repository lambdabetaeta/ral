//! The model in force and its own learned context limit.

use super::{Control, Recorded, Selected};

#[derive(Debug, PartialEq)]
pub struct Model {
    selected: Selected,
    narrowed: Option<u64>,
}

impl Model {
    pub(super) fn new(selected: Selected) -> Self {
        Self {
            selected,
            narrowed: None,
        }
    }
    pub fn selected(&self) -> &Selected {
        &self.selected
    }
    pub fn name(&self) -> &str {
        &self.selected.model
    }
    pub fn window(&self) -> Option<u64> {
        match (self.selected.context_window, self.narrowed) {
            (Some(listed), Some(narrowed)) => Some(listed.min(narrowed)),
            (listed, narrowed) => listed.or(narrowed),
        }
    }

    /// `model` is the model in force, and `limit` tightens what is known of its window.
    pub(crate) fn narrows(&self, model: &str, limit: u64) -> bool {
        model == self.name() && self.window().is_none_or(|window| limit < window)
    }

    pub(super) fn step(&mut self, record: &Recorded<&Control>) -> Result<(), String> {
        match record.value() {
            Control::SessionResumed { selected, .. } => {
                if self.name() != selected.model {
                    self.narrowed = None;
                }
                self.selected = selected.clone();
            }
            Control::ModelChanged { selected } => *self = Self::new(selected.clone()),
            Control::WindowNarrowed {
                model,
                context_window,
            } => {
                if !self.narrows(model, *context_window) {
                    return Err(format!(
                        "WindowNarrowed names {model:?} with limit {context_window}, but the model in force is {:?} with window {:?}; the limit must narrow that model",
                        self.name(),
                        self.window()
                    ));
                }
                self.narrowed = Some(*context_window);
            }
            Control::SessionStarted(_)
            | Control::JobOpened
            | Control::JobSettled { .. }
            | Control::Status { .. }
            | Control::Posted(_)
            | Control::Taken { .. }
            | Control::Withdrawn { .. }
            | Control::Scheduled { .. }
            | Control::Unscheduled { .. } => {}
        }
        Ok(())
    }
}
