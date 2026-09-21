An agent is a function from a prompt and your bindings to a value. It runs in a copy of your shell, reads what you point it at, and replies with first-order ral data that you compute with. 

`` exarch-agents `start [prompt: <Str>, name: <Str>, type: `amnemon|`mnemon, grant: <permission>, search: <Bool>, provider: `inherit|`named <Str>, model: `inherit|`named <Str>] `` asynchronously launches an agent. `name` is a descriptive handle ('todo-scan', 'chunk-3'). `grant` is one of `` `confined ``, `` `read-only ``, `` `edit-only ``, `` `reasonable ``, `` `dangerous ``, at most your own authority. An agent that only reads bindings and answers needs `` `confined ``. `search` says whether it may use the web. `explain exarch-agents` has the full documentation.

`provider` and `model` say what the child runs on; write `` `inherit `` for both to run it on your own selection. Spend a cheaper, faster model on a child whose task is mechanical and whose answer you will check — a scan, a summary, a fan-out over chunks — with `` model: `named '<model>' `` and `` provider: `inherit ``, which keeps your own account. Keep your own model for work that needs judgement.

An agent's shell is a snapshot of yours at `start`, and has exactly the same definitions and cwd. 

The agent's initial prompt may be constructed by using `ral`:

    let ctx = from-string < design.md
    exarch-agents `start [prompt: #'Review the plan bound at $ctx against src/solver/. Reply [verdict: `ok|`revise, issues: [[file, line, why]]].'#, name: 'plan-review', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]

Do not quote something that an agent can access from a binding. Never read a binding to echo it to an agent. The same goes for repeatable scripts, e.g. define `let run-suite = { … }` and ask the agent to run it by name. 

Agents end their session with a `ral` value (a record with fields, a list, a
variant). After receiving a notification you may access this value by ``
exarch-agents `read <name> ``. Bind it and project

    let r = exarch-agents `read 'plan-review'
    if !{equal $r[reply][verdict] `ok} { … } else { for $r[reply][issues] { |i| … } }

Example:

    let chunks = map { |f| from-string < $f } !{glob #'notes/*.md'#}
    for !{range 0 !{length $chunks}} { |i|
      exarch-agents `start [prompt: "Summarise the text bound at $chunks[$i]. Reply [topic: Str, claims: [Str]].", name: "chunk-$i", type: `amnemon, grant: `confined, search: false, provider: `inherit, model: `inherit]
    }

When all the agents are done:

    let parts = map { |i| let r = exarch-agents `read "chunk-$i"; $r[reply] } !{range 0 !{length $chunks}}

DO NOT POLL AGENTS. Wait to be notified of their completion.

`` exarch-agents `message [to: <name>, text: <text>] `` messages an agent. An agent that has replied remains idle for an hour, and can still be messaged to have its context re-used, if a similar task has come in. Any live agent may be messaged by name — a child of yours, a sibling, or whoever started you — and `` `list `` names them all, so you can reach anyone you can see.

There are two types of agents. `` `amnemon `` is the default; it begins a fresh session that sees only what your bindings and prompt carry. `` `mnemon `` forks your conversation with `prompt` as its final turn; use it only when the conversation itself is the input the child needs, and cannot be bound.

Every tag but `` `list `` and `` `read `` answers `` `summary [live: Int, replied: Int] `` afterwards: how many other agents are alive around you, and how many of the agents you started are parked holding a value you have not fetched. A non-zero `replied` means go and `` `read ``.

`` exarch-agents `list `` gives the rows, `[[name, spawner, state, idle-s, elapsed-s, log-dir]]` — every live agent in your tree, you included, not only what you started. `spawner` says who started each one: `` `root `` for an agent a human started, `` `agent <name> `` otherwise. `state` one of `` `busy ``, `` `waiting-on-agents ``, `` `replied ``, `` `waiting ``.

You may message anyone on that list, but only cancel or read an agent you started — one whose `spawner` is you, or that sits under one. To find just your own:

    filter { |r| equal $r[spawner] `agent 'my-name' } !{exarch-agents `list}

`` exarch-agents `cancel <name> `` stops an agent you started. It takes effect at the next tool boundary, so there might be some delay.
