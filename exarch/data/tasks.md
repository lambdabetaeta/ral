A task management kit is always loaded; use it to remember what to do next. The list lives on the "tasks" pin, not in a binding — every call reads and writes the register directly, so nothing to rebind and nothing that a block or `within` can discard. Mutate it from the foreground only: these calls read the register through an enquiry, so every one of them errors inside `spawn { … }`:

  add-task "do one thing"
  add-task "do second thing"

When you begin a task you only need to run

  transition 1 `doing  # before you begin

When that is finished, mark it as done and view the tasks:

  transition 1 `done
  render-tasks         # view remaining tasks

If at this point task 2 is no longer necessary, run

  remove-task 2        # task no longer necessary

Definitions:

  clear-tasks              — empty the task list
  add-task <desc>          — add a task with a fresh id
  transition <id> <status> — change status
  remove-task <id>         — drop a task
  note-task <id> <note>    — add notes to a task
  tag-task <id> <tag>      — add a tag to a task
  untag-task <id> <tag>    — remove a tag from a task
  retag-task <id> <tags>   — replace all tags
  render-tasks             — read tasks on stdout
  save-tasks <path> / load-tasks <path>

Schema: [ id: Int, desc: String, status: `open | `doing | `blocked | `done, tags: [String], notes: String ]

Anything bespoke — a field this kit does not surface, a one-off inspection — reads straight off the register: `pin-read "tasks"` answers the card itself, `unit` when the list is empty.

## Goal

Alongside the task list, hold the session's overarching aim so it survives across turns:

  set-goal "land the parser refactor"   # record the current goal; call again to replace
  clear-goal                            # drop it once the aim is met or abandoned

One goal is held at a time: `set-goal` replaces the previous, and `clear-goal` is idempotent (a quiet no-op when nothing is set). While a goal is set, the periodic reminder keeps it in front of you.
