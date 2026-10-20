//! The embedded agent library, `data/agent.ral`, and the docs it carries.

use ral_core::source::SourceOrigin;
use ral_core::types::{Break, Mooring, Settled, sig};
use ral_core::{Shell, Value};

const AGENT_SOURCE: &str = include_str!("../data/agent.ral");

/// Source the embedded agent helper library into `shell`. Its docs
/// ([`agent_library_docs`]) are the runtime's, installed by
/// `boot::engine_runtime`.
///
/// # Errors
/// If sourcing raises a ral error (re-surfaced as a signal) or escapes.
pub fn install_agent_library(mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let origin = SourceOrigin::named("<exarch:agent>");
    ral_core::load::evaluate_source(mooring, shell, AGENT_SOURCE, &origin, None).map_err(
        |e| match e {
            Break::Error(err) => sig(format!("exarch agent library: {}", err.message)),
            other @ Break::Escape(_) => other,
        },
    )
}

/// One-line docs for `agent.ral`'s helpers.  They are ral closures, not
/// registered builtins, so `help` cannot find them unaided;
/// `boot::engine_runtime` plants these in the runtime.
pub(crate) fn agent_library_docs() -> Vec<(String, String)> {
    [
        ("view-text-around", "view-text-around PATH LINE PEEK  — show the 2*PEEK+1 lines of PATH centred on LINE, in `view-text`'s records, clamped at the top of the file."),
        ("view-hash-around", "view-hash-around PATH LINE PEEK  — the same window in `view-hash`'s records, each carrying its witness."),
        ("exarch-tasks", "exarch-tasks <tag>  — your task list. Every tag returns the updated list, one record per task (id, desc, status, tags, notes).\n\nexarch-tasks `add <desc>  — append an `open task.\nexarch-tasks `remove <id>  — drop a task.\nexarch-tasks `clear  — empty the list; `done tasks stay until you do.\nexarch-tasks `list  — read the list.\nexarch-tasks `status [id: <Int>, status: `open|`doing|`blocked|`done]  — set a status.\nexarch-tasks `tag [id: <Int>, tag: <Str>]  — add a tag.\nexarch-tasks `untag [id: <Int>, tag: <Str>]  — remove a tag.\nexarch-tasks `note [id: <Int>, note: <Str>]  — set the notes.\nexarch-tasks `retag [id: <Int>, tags: [<Str>]]  — replace all tags."),
        ("exarch-goal", "exarch-goal <tag>  — your current goal.\n\nexarch-goal `set <text>  — replace it.\nexarch-goal `clear  — drop it; a no-op when none is set."),
    ]
    .into_iter()
    .map(|(n, d)| (n.to_string(), d.to_string()))
    .collect()
}
