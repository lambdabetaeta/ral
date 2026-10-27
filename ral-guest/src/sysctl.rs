//! Disable unprivileged user namespaces before any engine runs to preserve cross-uid isolation.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sysctl {
    pub(crate) path: &'static str,
    pub(crate) value: &'static str,
}

impl Sysctl {
    /// Failure is fatal: the jail depends on userns-off for cross-uid ptrace isolation.
    pub(crate) fn apply(&self) -> Result<(), String> {
        std::fs::write(self.path, self.value)
            .map_err(|err| format!("could not set {} to {}: {err}", self.path, self.value))
    }
}

/// Use the portable upstream control; a missing sysctl is a hard boot failure.
pub(crate) const PLAN: [Sysctl; 1] = [Sysctl {
    path: "/proc/sys/user/max_user_namespaces",
    value: "0",
}];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_disables_unprivileged_user_namespaces() {
        assert_eq!(
            PLAN,
            [Sysctl {
                path: "/proc/sys/user/max_user_namespaces",
                value: "0",
            }]
        );
    }
}
