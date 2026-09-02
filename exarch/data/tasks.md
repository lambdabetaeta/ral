A task management kit is always loaded; use it to remember what to do next. The list lives on the "tasks" pin, not in a binding — every call reads and writes the register directly, so nothing to rebind and nothing that a block or `within` can discard. Mutate it from the foreground only: these calls read the register through an enquiry, so every one of them fails — not inside `spawn { … }`, and not as a stage of a `|` pipeline:

  tasks-add "do one thing"
  tasks-add "do second thing"

When you begin a task you only need to run

  tasks-status 1 `doing  # before you begin

When that is finished, mark it as done and view the tasks:

  tasks-status 1 `done
  tasks-list           # the tasks, as ral records

If at this point task 2 is no longer necessary, run

  tasks-remove 2        # task no longer necessary

Definitions:

  tasks-list               — the task list as ral records
  tasks-clear              — empty the task list
  tasks-add <desc>         — add a task with a fresh id
  tasks-status <id> <status> — change status
  tasks-remove <id>        — drop a task
  tasks-note <id> <note>   — add notes to a task
  tasks-tag <id> <tag>     — add a tag to a task
  tasks-untag <id> <tag>   — remove a tag from a task
  tasks-retag <id> <tags>  — replace all tags
  tasks-save <path> / tasks-load <path>

Schema: [ id: Int, desc: String, status: `open | `doing | `blocked | `done, tags: [String], notes: String ]

Anything bespoke — a field this kit does not surface, a one-off inspection — reads straight off the register: `pin-read "tasks"` answers the card itself, `()` when the list is empty. Normally, though, `tasks-list` is how you get records, and the kit deliberately ships no query functions: query with `filter`/`first` over it, e.g. `filter { |t| equal $t[status] \`doing } !{tasks-list}`.

## Goal

Alongside the task list, hold the session's overarching aim so it survives across turns:

  goal-set "land the parser refactor"   # record the current goal; call again to replace
  goal-clear                            # drop it once the aim is met or abandoned

One goal is held at a time: `goal-set` replaces the previous, and `goal-clear` is idempotent (a quiet no-op when nothing is set). While a goal is set, the periodic reminder keeps it in front of you.
