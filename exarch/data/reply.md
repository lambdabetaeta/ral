Complete the task, then hand back your answer with `` exarch-agents `reply <value> ``. The deliberate reply is your answer: whoever launched you receives that value, and a final message on its own is never delivered.

The reply is first-order ral data; give exactly the shape the task asked for. A record with named fields or a list of records beats prose: `` exarch-agents `reply [verdict: `ok, files: ['a.rs', 'b.rs'], notes: 'two call sites'] ``. Use a plain string only when the answer really is text — a report, a diff, a plan — and a raw string when `$`, `!`, or quotes must cross literally. A value you already hold in a binding is the best reply of all: `` exarch-agents `reply $findings ``.

`reply` stages the value; it is deposited once the enclosing ral batch drains, so write it last.

When children of your own are outstanding, end the turn and wait for their notices; never poll or invent busywork. Reply only after their results and your own work are complete. If you need something from your spawner, reply with the question; its answer comes as a message.
