//! `$HOME` lookup, single source of truth.
//!
//! Every grant path that begins with `~` or `xdg:` is resolved
//! against the user's home directory, so there must be one — and
//! only one — answer to "what is HOME".  This module owns it.
//!
//! Resolution order:
//!
//!   1. The dynamic env, if the caller has one (`within [shell:
//!      HOME=…]` overrides flow through here).
//!   2. The process env's `HOME` (Unix and most CI / Docker).
//!   3. The process env's `USERPROFILE` (Windows callers).
//!   4. Empty string.  Callers that need a real path
//!      (`freeze_path_list`, `expand_path_prefix`) error
//!      explicitly on empty rather than silently producing a
//!      bogus result.

use std::collections::HashMap;

/// Look up `HOME`, preferring the supplied dynamic env overrides.
/// Empty string when nothing is set.
pub fn home(env_overrides: &HashMap<String, String>) -> String {
    env_overrides
        .get("HOME")
        .cloned()
        .or_else(|| std::env::var("HOME").ok())
        .or_else(|| std::env::var("USERPROFILE").ok())
        .unwrap_or_default()
}

/// Look up `HOME` from the process env only — for callers that
/// have no dynamic env at hand (REPL completion, exarch policy
/// loaders that run before any `Dynamic` exists).
pub fn home_from_env() -> String {
    home(&HashMap::new())
}
