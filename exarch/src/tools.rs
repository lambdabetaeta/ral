//! `ral` — exarch's one tool.
//!
//! Every other harness affordance the model reaches — spawning a sub-agent,
//! messaging one, scheduling a wakeup, opening/verifying a commitment,
//! replying — is an ordinary ral builtin (`agent_builtins/harness.rs`) that
//! speaks to the host through the enquiry desk ([`crate::fleet::desk`]). This
//! module holds only what remains a genuine sibling of that split: `ral`
//! itself (the one call that crosses the provider boundary; see [`ral`])
//! and the spawn plumbing every launch shares (`/branch`'s [`spawn_branch`]
//! and the desk's own spawns, both built on [`agent::spawn_async`]).

pub(crate) mod agent;
pub(crate) mod ral;

pub(crate) use agent::spawn_branch;
pub(crate) use crate::fleet::desk::CommitmentSettle;
pub(crate) use ral::wire_tool;
