A task kit is always loaded; use it to remember what to do next. The list lives on the "tasks" pin, so it outlives blocks and `within`; call it from the foreground only, never inside `spawn { … }` or as a `|` stage.

Every tag answers the list after the change, one record `[ id: Int, desc: String, status: `open | `doing | `blocked | `done, tags: [String], notes: String ]` per task:

    exarch-tasks `add <desc> / `remove <id> / `clear / `list
    exarch-tasks `status [id, status: `open|`doing|`blocked|`done]
    exarch-tasks `tag [id, tag] / `untag [id, tag] / `note [id, note] / `retag [id, tags]

Mark a task `doing before you start it and `done when it is finished. Done tasks stay listed until you `clear. An invalid status warns and changes nothing. Query with `filter`/`first-where`, e.g. `filter { |t| equal $t[status] \`doing } !{exarch-tasks \`list}`.

## Goal

Alongside the task list, hold the session's overarching aim so it survives across turns:

    exarch-goal `set <text> / `clear

`` `set `` replaces the previous goal; `` `clear `` is idempotent (a quiet no-op when nothing is set). While a goal is set, the periodic reminder keeps it in front of you.
