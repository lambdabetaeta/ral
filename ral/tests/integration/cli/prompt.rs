//! The interactive prompt, driven through a real pty.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod editor;
mod lifecycle;
mod termination;
