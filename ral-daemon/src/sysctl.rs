//! The one guest-wide kernel setting the engine's spawn jail depends on:
//! unprivileged user namespaces off.
//!
//! The jail drops a spawned command to a fresh, unprivileged uid inside
//! its own cgroup; an unprivileged process that could still
//! `unshare(CLONE_NEWUSER)` would gain a namespace in which it is "root"
//! enough to attach a ptrace to a sibling, defeating the cross-uid
//! isolation the jail exists for.  Turning the capability off before the
//! engine — or anything it spawns — ever runs closes that door.  Same
//! pure-plan/thin-apply split as [`crate::mounts`]: [`plan`] is data,
//! [`Sysctl::apply`] is the one syscall.

/// One `/proc/sys` setting, as data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sysctl {
    pub path: &'static str,
    pub value: &'static str,
}

impl Sysctl {
    /// Write this setting.  Unlike [`crate::mounts::Mount::apply`], there
    /// is no warn-and-continue here: userns-off is a load-bearing security
    /// guarantee the jail's whole cross-uid-ptrace argument depends on,
    /// not a diagnostic.
    ///
    /// # Errors
    /// Returns a sentence naming the setting that failed and the reason
    /// the kernel gave.
    pub fn apply(&self) -> Result<(), String> {
        std::fs::write(self.path, self.value)
            .map_err(|err| format!("could not set {} to {}: {err}", self.path, self.value))
    }
}

/// The one setting that must be in place before the engine — or anything
/// it spawns — runs.
///
/// `user.max_user_namespaces`, the portable upstream control, is used
/// rather than Ubuntu's own Debian-patch `kernel.unprivileged_userns_clone`:
/// it is always present on the pinned kernel, and writing to a sysctl that
/// does not exist should be a hard boot failure, not a silent skip — so
/// the name that is guaranteed to resolve is the right one to depend on.
pub fn plan() -> Vec<Sysctl> {
    vec![Sysctl {
        path: "/proc/sys/user/max_user_namespaces",
        value: "0",
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plan names exactly the one portable control, never Ubuntu's
    /// own Debian-patch sysctl.
    #[test]
    fn the_plan_disables_unprivileged_user_namespaces() {
        let settings = plan();
        assert_eq!(
            settings,
            vec![Sysctl {
                path: "/proc/sys/user/max_user_namespaces",
                value: "0",
            }]
        );
    }
}
