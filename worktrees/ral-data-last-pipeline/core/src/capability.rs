//! Runtime capability decisions over the dynamic capability stack.
//!
//! This module owns every yes/no answer the runtime asks of the
//! capability stack — exec, fs, editor, shell, and the OS-renderable
//! `SandboxProjection` — through a single front door, `EffectiveGrant`.
//! The capability *types* (`Capabilities`, `ExecPolicy`, `FsPolicy`, …)
//! live in `crate::types::capability`.
//!
//! ## Sub-modules
//!
//! - `effective` — `EffectiveGrant`: the single decision authority.
//! - `check`     — internal stack-walk helpers backing `EffectiveGrant`.
//! - `exec`      — per-layer and stack-level exec verdict evaluation.
//! - `prefix`    — `GrantPath` and prefix-set intersection helpers.
//!
//! The `Meet` and `Join` traits live alongside the types they operate
//! on, in [`crate::types::capability`].

mod check;
mod effective;
mod exec;
mod prefix;

pub(crate) use effective::EffectiveGrant;
