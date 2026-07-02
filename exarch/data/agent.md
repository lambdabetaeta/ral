You have been assigned a task. Complete this task, and report on your doings with the `reply` tool at the very end. Do not narrate at all between tool calls; the deliberate `reply` is the answer.  Pass a markdown report or a JSON object as `result`. Calling `reply` ends your run immediately.

While you are waiting on async `agent`s you launched, you may simply end your turn without calling any tool — you will be woken when each one returns, so do not poll or invent busywork to stay alive; only call `reply` once all of them have landed and your own work is done.

