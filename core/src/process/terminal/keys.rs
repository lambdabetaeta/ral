//! Attribution of a tenant's reported terminal gestures to the run's node.

use crate::process::Signal;
use crate::process::cancel::{CancelCause, ScopeNode};
use crate::process::signal::gesture;

pub(super) struct KeyAttribution {
    home: ScopeNode,
    pressed: Option<CancelCause>,
}

impl KeyAttribution {
    pub(super) fn new(home: ScopeNode) -> Self {
        Self {
            home,
            pressed: None,
        }
    }

    /// Strike each reported key at once; retain the strongest for teardown.
    pub(super) fn hear(&mut self, signal: Signal) {
        if let Some(key) = gesture(signal) {
            self.pressed = self.pressed.max(Some(key));
            self.home.cancel(key);
        }
    }

    pub(super) fn pressed(&self) -> Option<CancelCause> {
        self.pressed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_reported_key_strikes_its_home_and_keeps_the_strongest() {
        let home = crate::process::DurableRoot::new().child();
        let mut attribution = KeyAttribution::new(home.clone());
        assert_eq!(attribution.pressed(), None);
        attribution.hear(Signal::new(libc::SIGTERM));
        assert_eq!(home.cause(), None);
        assert_eq!(attribution.pressed(), None);
        attribution.hear(Signal::new(libc::SIGINT));
        assert_eq!(home.cause(), Some(CancelCause::Interrupted));
        assert_eq!(attribution.pressed(), Some(CancelCause::Interrupted));
        attribution.hear(Signal::new(libc::SIGQUIT));
        attribution.hear(Signal::new(libc::SIGINT));
        assert_eq!(home.cause(), Some(CancelCause::Aborted));
        assert_eq!(attribution.pressed(), Some(CancelCause::Aborted));
    }

    #[cfg(windows)]
    #[test]
    fn a_console_death_reports_no_new_key() {
        let home = crate::process::DurableRoot::new().child();
        let mut attribution = KeyAttribution::new(home.clone());
        attribution.hear(Signal::new(
            windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT,
        ));
        assert_eq!(home.cause(), None);
        assert_eq!(attribution.pressed(), None);
    }
}
