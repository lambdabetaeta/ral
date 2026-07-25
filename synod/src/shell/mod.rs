//! The desktop shell — the binary's own half of the crate, private to it.
//!
//! Where the library modules ([`grant`](crate::grant),
//! [`prompt`](crate::prompt), [`session`](crate::session),
//! [`workspace`](crate::workspace)) are the engine anyone could drive, these
//! are the Tauri window that drives it: the commands the window calls
//! ([`commands`]), the `ChatGPT` sign-in the opening screen offers
//! ([`signin`]), the change report and undo actions ([`review`]), and the
//! bridge that streams the conversation's narration into the window
//! ([`sink`]).  Nothing here is part of `synod`'s public surface.

pub mod commands;
pub mod review;
pub mod signin;
pub mod sink;
