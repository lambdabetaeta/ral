Agents run delegated subtasks in their own shell and model context, then return structured values. Use them for independent investigations, reviews, or edits you can specify and check.

Prepare inputs in bindings, including reusable helper blocks. A child receives a snapshot of your bindings, cwd, and environment at start; later definitions do not propagate. Pass binding names in raw prompts, not copies of their contents:

    let plan = from-string < #'design.md'#
    let inspect-diff = { git diff -- src/solver/ }

Give the child a bounded task and an explicit reply shape. Use `amnemon` when you can describe the task in a prompt and point to current shell bindings or files; use `mnemon` when it needs the entire conversation up to this point:

    exarch-agents `start [
      prompt: #'Review $plan against src/solver/ and the changes returned by inspect-diff. Identify correctness issues; do not edit. Reply [verdict: `ok|`revise, issues: [[file: Str, line: Int, why: Str]]].'#,
      name: #'plan-review'#, type: `amnemon, grant: `read-only,
      model: `inherit
    ]

Do independent work while the child runs. Do not poll: if nothing remains, end your turn and wait for its notice, as for a `spawn`ed worker. Then bind the reply and inspect the fields you need. Their presence and types are checked at `read`:

    let review = exarch-agents `read #'plan-review'#
    [verdict: $review[reply][verdict], issues: $review[reply][issues]]

Message a running agent to correct its direction, supply a discovery, or change its scope. You can also message another working agent in your tree, including your parent or siblings. A resting helper accepts messages only from its parent or the human:

    exarch-agents `message [
      to: #'plan-review'#, text: #'The empty-input behavior is intentional. Focus on whether the new duplicate check changes error precedence.'#
    ]

Messages between agents are notes, except your messages to your own helpers: each is owed a reply. A helper that needs something from you replies with its question; answer it with a follow-up message.

Reuse a replied child for related work: it retains its context and shell. A helper rests for five hours by default (`--helper-idle` sets the limit), then ends if it has nothing it may take up. A new message from you resumes work. Wait for its next notice before reading again: `read` refuses while it works:

    exarch-agents `message [
      to: #'plan-review'#, text: #'Check src/parser/ for the same error-precedence issue. Return the same shape.'#
    ]

Files are shared. Give concurrent editors non-overlapping write grants when starting them; each child's system prompt shows its grant:

    exarch-agents `start [
      prompt: #'Fix the issues in $review[reply][issues] affecting src/solver/. Reply [changed: [Str], summary: Str].'#,
      name: #'solver-edit'#, type: `amnemon,
      grant: `restrict [fs: [read: [`cwd #'.'#], write: [`cwd #'src/solver/'#]]],
      model: `inherit
    ]

Inspect the changes after the editors reply:

    git diff -- src/solver/ src/parser/

Stop a descendant's current exchange without ending it or its helpers:

    exarch-agents `stop #'plan-review'#

Dismiss one of your own helpers when its life is no longer needed. This ends and joins it and its descendants before returning:

    exarch-agents `dismiss #'plan-review'#

Reply and Stop preserve your helpers. A human may resume a resting helper quietly: you see its published status, but receive no model notice for that work.

For permissions, fan-out, and result schemas, consult the command guide:

    explain exarch-agents
