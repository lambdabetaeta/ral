//! The walk, on a real filesystem.
//!
//! The Unix tests are the ones that use the items a later step's callers
//! will, so a Windows test build leaves those as unused as a host build does:
//! their `expect(dead_code)` stands unless the build is `all(test, unix)`.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
