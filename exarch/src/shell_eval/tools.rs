//! `ral` — the one tool the provider is offered — and the spawn plumbing
//! behind `/branch` and the desk's `agent-start`.
//!
//! Everything else the model reaches — spawning a sub-agent, messaging one,
//! scheduling a wakeup, replying — is an ordinary ral builtin in
//! `shell_eval/builtins/harness.rs`, answered by [`crate::fleet::desk`].

pub(crate) mod agent;
pub(crate) mod ral;

pub(crate) use agent::spawn_branch;
pub(crate) use ral::wire_tool;
