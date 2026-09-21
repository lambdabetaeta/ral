A task management kit is always loaded; use it to remember what to do next. The list lives on the "tasks" pin, not in a binding — every call reads and writes the register directly, so nothing to rebind and nothing that a block or `within` can discard. Mutate it from the foreground only: `exarch-tasks` reads the register through an enquiry, so every call fails inside `spawn { … }`, and as a stage of a `|` pipeline.

Every tag of `exarch-tasks` answers the task list after the transition — a mutation and a read are the same call, so `` `list `` is just the tag that transitions nothing:

  exarch-tasks `add "do one thing"
  exarch-tasks `add "do second thing"
  exarch-tasks `status [id: 1, status: `doing]   # before you begin
  exarch-tasks `status [id: 1, status: `done]
  exarch-tasks `remove [id: 2]                    # task no longer necessary
  exarch-tasks `list                              # the tasks, as ral records

    exarch-tasks `add <desc> / `remove <id> / `clear / `list
    exarch-tasks `status [id: Int, status: `open|`doing|`blocked|`done]
    exarch-tasks `tag [id, tag] / `untag [id, tag] / `note [id, note]
    exarch-tasks `retag [id, tags] / `save <path> / `load <path>

Schema: `[ id: Int, desc: String, status: `open | `doing | `blocked | `done, tags: [String], notes: String ]`. An invalid `status` `warn`s and answers the list unchanged.

Anything bespoke — a field this kit does not surface, a one-off inspection — reads straight off the register: `` exarch-pins `read "tasks" `` answers the card itself, `()` when the list is empty. Normally, though, `` exarch-tasks `list `` is how you get records, and the kit deliberately ships no query functions: query with `filter`/`first` over it, e.g. `filter { |t| equal $t[status] \`doing } !{exarch-tasks \`list}`.

## Goal

Alongside the task list, hold the session's overarching aim so it survives across turns:

    exarch-goal `set <text> / `clear

`` `set `` replaces the previous goal; `` `clear `` is idempotent (a quiet no-op when nothing is set). While a goal is set, the periodic reminder keeps it in front of you.
