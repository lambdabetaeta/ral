//! Booked wakeups, independent of the timers made from them.

use super::{Control, Recorded, Schedule};
use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Eq)]
pub struct Schedules(BTreeMap<String, Schedule>);

impl Schedules {
    pub(super) fn new() -> Self {
        Self(BTreeMap::new())
    }
    pub fn get(&self, label: &str) -> Option<&Schedule> {
        self.0.get(label)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Schedule)> {
        self.0
            .iter()
            .map(|(label, schedule)| (label.as_str(), schedule))
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn step(&mut self, record: &Recorded<&Control>) -> Result<(), String> {
        match record.value() {
            Control::Scheduled { label, schedule } => {
                drop(self.0.insert(label.clone(), schedule.clone()));
            }
            Control::Unscheduled { label } => {
                drop(
                    self.0
                        .remove(label)
                        .ok_or_else(|| format!("no schedule bears label {label:?}"))?,
                );
            }
            Control::SessionStarted(_)
            | Control::SessionResumed { .. }
            | Control::JobOpened
            | Control::JobSettled { .. }
            | Control::Status { .. }
            | Control::Posted(_)
            | Control::Taken { .. }
            | Control::Withdrawn { .. }
            | Control::ModelChanged { .. }
            | Control::WindowNarrowed { .. } => {}
        }
        Ok(())
    }
}
