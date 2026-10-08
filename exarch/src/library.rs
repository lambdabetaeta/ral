//! The embedded agent library, `data/agent.ral`, and the docs it carries.

use ral_core::types::{Break, Mooring, Settled, sig};
use ral_core::{Shell, Value};

const AGENT_SOURCE: &str = include_str!("../data/agent.ral");

/// Source the embedded agent helper library into `shell`, installing its one-line
/// docs ([`agent_library_docs`]) in the same act, so `help` can never list a
/// helper the shell lacks nor miss one it has.
///
/// # Errors
/// If sourcing raises a ral error (re-surfaced as a signal) or escapes.
pub fn install_agent_library(mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let result =
        ral_core::load::evaluate_source(mooring, shell, AGENT_SOURCE, "<exarch:agent>", None)
            .map_err(|e| match e {
                Break::Error(err) => sig(format!("exarch agent library: {}", err.message)),
                other @ Break::Escape(_) => other,
            })?;
    shell.install_library_docs(agent_library_docs());
    Ok(result)
}

/// One-line docs for `agent.ral`'s helpers.  They are ral closures, not
/// registered builtins, so `help` cannot find them unaided;
/// [`install_agent_library`] plants these in the sourcing shell's session.
pub(crate) fn agent_library_docs() -> Vec<(String, String)> {
    [
        ("view-text-around", "view-text-around PATH LINE PEEK  — show the 2*PEEK+1 lines of PATH centred on LINE, in `view-text`'s records, clamped at the top of the file."),
        ("view-hash-around", "view-hash-around PATH LINE PEEK  — the same window in `view-hash`'s records, each carrying its witness."),
        ("exarch-tasks", "exarch-tasks <tag>  — your task list: `add a task, `remove one, `clear the whole list, `list to read it; `status`, `tag`, `untag`, `note`, and `retag` edit one task by id; `save`/`load` file it as JSON. Every tag answers the task list after the change, one record per task (id, desc, status, tags, notes), even `list, which changes nothing. A `status` this family does not recognise draws a warning and leaves the list unchanged rather than failing the call.\n\nexarch-tasks `add <desc>  — allocate a fresh id and append a task with status `open.\nexarch-tasks `remove <id>  — drop a task by id.\nexarch-tasks `clear  — empty the task list.\nexarch-tasks `list  — read the task list; changes nothing.\nexarch-tasks `status [id: <Int>, status: `open|`doing|`blocked|`done]  — change a task's status.\nexarch-tasks `tag [id: <Int>, tag: <Str>]  — add a tag to a task.\nexarch-tasks `untag [id: <Int>, tag: <Str>]  — remove a tag from a task.\nexarch-tasks `note [id: <Int>, note: <Str>]  — set a task's notes.\nexarch-tasks `retag [id: <Int>, tags: [<Str>]]  — replace all of a task's tags.\nexarch-tasks `save <path>  — write the task list to PATH as JSON.\nexarch-tasks `load <path>  — read a task list from PATH as JSON, replacing the current one."),
        ("exarch-goal", "exarch-goal <tag>  — the one goal statement you keep in view: `set <text>` writes it, `clear` empties it."),
    ]
    .into_iter()
    .map(|(n, d)| (n.to_string(), d.to_string()))
    .collect()
}
