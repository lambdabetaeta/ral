A task management kit is always loaded; use it to remember what to do next. Store tasks in `$exarch-tasks`, and update them across turns:

  let exarch-tasks = $empty-tasks
  let exarch-tasks = add-task $exarch-tasks "do one thing"
  let exarch-tasks = add-task $exarch-tasks "do second thing"

When you begin a task you only need to run

  let exarch-tasks = transition $exarch-tasks 1 `doing  # before you begin

When that is finished, mark it as done and view the tasks:

  let exarch-tasks = transition $exarch-tasks 1 `done
  render-tasks $exarch-tasks                           # view remaining tasks

If at this point task 2 is no longer necessary, run

  let exarch-tasks = remove-task $exarch-tasks 2       # task no longer necessary

Definitions - remember to use immutably:

  $empty-tasks                            — empty task list
  add-task $exarch-tasks <desc>           — add a task with a fresh id
  transition $exarch-tasks <id> <status>  — change status
  remove-task $exarch-tasks <id>          — drop a task
  note-task $exarch-tasks <id> <note>     — add notes to a task
  tag-task $exarch-tasks <id> <tag>       — add a tag to a task
  untag-task $exarch-tasks <id> <tag>     — remove a tag from a task
  retag-task $exarch-tasks <id> <tags>    — replace all tags
  render-tasks $exarch-tasks              — read tasks on stdout
  save-tasks $exarch-tasks <path> / load-tasks <path>

Schema: [ id: Int, desc: String, status: `open | `doing | `blocked | `done, tags: [String], notes: String ]

## Goal

Alongside the task list, hold the session's overarching aim so it survives across turns:

  set-goal "land the parser refactor"   # record the current goal; call again to replace
  clear-goal                            # drop it once the aim is met or abandoned

One goal is held at a time: `set-goal` replaces the previous, and `clear-goal` is idempotent (a quiet no-op when nothing is set). While a goal is set, the periodic reminder keeps it in front of you.
