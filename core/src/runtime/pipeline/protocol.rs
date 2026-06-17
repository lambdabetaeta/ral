//! Subprocess gate / report protocol for process-staged pipelines.
//!
//! Every stage helper ral spawns runs behind a one-frame gate: the child
//! blocks reading the gate's channel until the parent has finished
//! spawning every stage and (for foreground pipelines) called
//! `tcsetpgrp`; the parent then writes the frame and the child unblocks.
//! [`FrameGate<T>`] is the parent side of that gate, generic over the
//! frame type.  [`FrameReader<T>`] is the dual: a detached background
//! thread that reads one frame from a child-side channel.
//!
//! [`HelperProtocol`] composes those primitives for ral helper stages
//! (carrying [`StageJob`] in and a
//! [`ChildEvalResponse`](crate::child_eval::ChildEvalResponse) back, plus
//! optional value-channel ends).
//!
//! ## Backend boundary
//!
//! Channel transport is platform-specific.  On Unix the channel is a
//! Unix-domain socketpair and inheritance into the child is by raw fd
//! (env var carries the fd number, child re-opens via `from_raw_fd`).
//! On Windows the channel is an anonymous-pipe Reader/Writer pair and
//! inheritance is by `SetHandleInformation(HANDLE_FLAG_INHERIT)` plus
//! a numeric handle value stashed in env (the helper parses it as a
//! `HANDLE` and wraps it via `from_raw_handle`).  Both backends present
//! the same minimal API: `platform::Channel` (the channel type),
//! `platform::pair` (allocate one), `platform::pass` (mark
//! inheritable + stash in env on a `Command`), `platform::reader`
//! (spawn a [`FrameReader`]).  Common code (the gate / reader /
//! pending-frame primitives) is generic over `platform::Channel`
//! and lives in [`common`].

mod common;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
// wasm and other targets that are neither Unix nor Windows: the windows
// backend's `windows_sys` / `os_pipe` imports are `cfg(windows)`-only, so
// it cannot stand in here.  A stub backend keeps the module compiling,
// matching the `cfg(not(any(unix, windows)))` stubs in `process::signal`.
#[cfg(not(any(unix, windows)))]
mod fallback;

#[cfg(not(any(unix, windows)))]
use fallback as platform;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

pub(super) use common::{FrameReader, HelperProtocol, PendingFrame, pipe_error};

#[cfg(unix)]
pub(super) use unix::{Channel as ValueChannel, pair as create_value_pair, pass};
// `pass` is only consumed by Unix-only call sites today (the anchor
// process); on the other backends it stays available in case a future
// caller needs it but is not currently invoked.
#[cfg(not(any(unix, windows)))]
#[allow(unused_imports)]
pub(super) use fallback::{Channel as ValueChannel, pair as create_value_pair, pass};
#[cfg(windows)]
#[allow(unused_imports)]
pub(super) use windows::{Channel as ValueChannel, pair as create_value_pair, pass};

use super::helper::StageJob;

/// The concrete gate-frame type queued by the launcher until
/// `claim_foreground` completes, then released in one pass.
pub(super) type DeferredFrame = PendingFrame<StageJob>;
