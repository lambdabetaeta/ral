An agent is a function from a prompt and your bindings to a value. It runs in a copy of your shell, reads what you point it at, and replies with first-order ral data that you compute with. 

`` agents `start [prompt: <Str>, name: <Str>, type: `amnemon|`mnemon, grant: <permission>, search: <Bool>] `` asynchronously launches an agent. `name` is a descriptive handle ('todo-scan', 'chunk-3'). `grant` is one of `` `confined ``, `` `read-only ``, `` `edit-only ``, `` `reasonable ``, `` `dangerous ``, at most your own authority. An agent that only reads bindings and answers needs `` `confined ``. `search` says whether it may use the web. `explain agents` has the full documentation.

An agent's shell is a snapshot of yours at `start`, and has exactly the same definitions and cwd. 

The agent's initial prompt may be constructed by using `ral`:

    let ctx = from-string < design.md
    agents `start [prompt: #'Review the plan bound at $ctx against src/solver/. Reply [verdict: `ok|`revise, issues: [[file, line, why]]].'#, name: 'plan-review', type: `amnemon, grant: `read-only, search: false]

Do not quote something that an agent can access from a binding. Never read a binding to echo it to an agent. The same goes for repeatable scripts, e.g. define `let run-suite = { … }` and ask the agent to run it by name. 

Agents end their session with a `ral` value (a record with fields, a list, a
variant). After receiving a notification you may access this value by ``
agents `read <name> ``. Bind it and project

    let r = agents `read 'plan-review'
    if !{equal $r[reply][verdict] `ok} { … } else { for $r[reply][issues] { |i| … } }

Example:

    let chunks = map { |f| from-string < $f } !{glob #'notes/*.md'#}
    for !{range 0 !{length $chunks}} { |i|
      agents `start [prompt: "Summarise the text bound at $chunks[$i]. Reply [topic: Str, claims: [Str]].", name: "chunk-$i", type: `amnemon, grant: `confined, search: false]
    }

When all the agents are done:

    let parts = map { |i| let r = agents `read "chunk-$i"; $r[reply] } !{range 0 !{length $chunks}}

DO NOT POLL AGENTS. Wait to be notified of their completion.

`` agents `message [to: <name>, text: <text>] `` messages an agent. An agent that has replied remains idle for an hour, and can still be messaged to have its context re-used. If a similar task has come in.

There are two types of agents. `` `amnemon `` is the default; it begins a fresh session that sees only what your bindings and prompt carry. `` `mnemon `` forks your conversation with `prompt` as its final turn; use it only when the conversation itself is the input the child needs, and cannot be bound.

Every tag but `` `read `` answers with a roster of agents afterwards, `[[name, state, idle-s, elapsed-s, log-dir]]`. `state` one of `` `busy ``, `` `waiting-on-agents ``, `` `replied ``, `` `waiting ``. `` agents `list `` shows it, and `` agents `cancel <name> `` stops one. A `` `cancel `` will take effect at the next tool boundary, so there might be some delay.
