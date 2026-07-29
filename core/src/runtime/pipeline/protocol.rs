//! Gate / report protocol for ral helper stages, parent side.
//!
//! A helper stage blocks reading its gate channel until the parent has
//! spawned every stage and, for a foreground pipeline, called
//! `tcsetpgrp`; the parent then writes every gate frame in one pass, so
//! the stages start together.  `common` holds the gate / reader /
//! pending-frame primitives, generic over the backend's `Channel`; a
//! backend supplies just that type plus `pair` (allocate a channel) and
//! `pass` (hand one end to a child, named through an env var).

mod common;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

use common::PendingFrame;
pub(super) use common::{FrameReader, HelperProtocol, pipe_error};

#[cfg(unix)]
pub(super) use unix::{Channel as ValueChannel, pair as create_value_pair, pass};
// Only `pass` goes unused on Windows: its one caller outside this module is
// the anchor process in `group.rs`, which is Unix-only.
#[cfg(windows)]
#[allow(unused_imports)]
pub(super) use windows::{Channel as ValueChannel, pair as create_value_pair, pass};

use crate::child_eval::ChildEvalRequest;

/// The gate frame the launcher holds until `claim_foreground`, then releases.
pub(super) type DeferredFrame = PendingFrame<ChildEvalRequest>;
