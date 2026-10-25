//! Linux procfs names for pinned file descriptors.

/// `/proc/self/fd/<raw>` as a `PathBuf`: how a binary `sandbox::reexec`
/// pinned by fd (ral, bwrap) is exec'd, by the parent from the pin and by
/// bwrap from the slot the pin was lent at.
pub(crate) fn proc_fd_path(raw: std::os::fd::RawFd) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/proc/self/fd/{raw}"))
}
