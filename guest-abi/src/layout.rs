pub const WORK: &str = "/work";
pub const SCRATCH: &str = "/tmp";
pub const SHARE_TAG: &str = "shares";
/// Engines and their not-yet-jailed children; PID 1 stays outside this tree.
pub const ENGINE_CGROUP: &str = "/sys/fs/cgroup/ral-engines";
/// Kept commands live in `engine-<pid>/exec-<seq>`; survivors share `detached`.
pub const EXEC_CGROUP: &str = "/sys/fs/cgroup/ral-exec";
pub const EXEC_CONTROLLERS: &str = "+memory +pids +cpu";
pub const ENGINE_FLAG: &str = "--guest-engine";
pub const ENGINE_FD: i32 = 3;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_and_exec_cgroup_roots_are_distinct_siblings() {
        let (engine_parent, engines) = ENGINE_CGROUP.rsplit_once('/').unwrap();
        let (exec_parent, commands) = EXEC_CGROUP.rsplit_once('/').unwrap();
        assert_ne!(engines, commands);
        assert_eq!(engine_parent, exec_parent);
        assert_eq!(engine_parent, "/sys/fs/cgroup");
    }
}
