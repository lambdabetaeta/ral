Agents run delegated subtasks in their own shell and model context, then return structured values. Use them for independent investigations, reviews, or edits you can specify and check.

Prepare inputs in bindings, including reusable helper blocks. A child receives a snapshot of your bindings, cwd, and environment at start; later definitions do not propagate. Pass binding names in raw prompts, not copies of their contents:

    let plan = from-string < #'design.md'#
    let inspect-diff = { git diff -- src/solver/ }

Give the child a bounded task and an explicit reply shape. Use `amnemon` when you can describe the task in a prompt and point to current shell bindings or files; use `mnemon` when it needs the entire conversation up to this point. Inherit the provider and model unless the task calls for another selection:

    exarch-agents `start [
      prompt: #'Review $plan against src/solver/ and the changes returned by inspect-diff. Identify correctness issues; do not edit. Reply [verdict: `ok|`revise, issues: [[file: Str, line: Int, why: Str]]].'#,
      name: #'plan-review'#, type: `amnemon, grant: `read-only,
      provider: `inherit, model: `inherit
    ]

Do independent work while the child runs. Do not poll: if nothing remains, end your turn and wait for its notice. Then bind the reply and inspect the fields you need. Their presence and types are checked at `read`:

    let review = exarch-agents `read #'plan-review'#
    [verdict: $review[reply][verdict], issues: $review[reply][issues]]

Message a running agent to correct its direction, supply a discovery, or change its scope. You can also message another live agent in your tree, including your parent or siblings:

    exarch-agents `message [
      to: #'plan-review'#, text: #'The empty-input behavior is intentional. Focus on whether the new duplicate check changes error precedence.'#
    ]

Reuse a replied child for related work: it retains its context and shell. Each reply starts a one-hour follow-up window; a new message resumes work, and its next reply starts a fresh window. Wait for the new notice before reading again, since `read` still exposes the previous reply while it works:

    exarch-agents `message [
      to: #'plan-review'#, text: #'Check src/parser/ for the same error-precedence issue. Return the same shape.'#
    ]

Files are shared. Give concurrent editors non-overlapping write grants when starting them; each child's system prompt shows its grant:

    exarch-agents `start [
      prompt: #'Fix the issues in $review[reply][issues] affecting src/solver/. Reply [changed: [Str], summary: Str].'#,
      name: #'solver-edit'#, type: `amnemon,
      grant: `restrict [fs: [read: [`cwd #'.'#], write: [`cwd #'src/solver/'#]]],
      provider: `inherit, model: `inherit
    ]

Inspect the changes after the editors reply:

    git diff -- src/solver/ src/parser/

Cancel a child whose work is no longer needed. Cancellation also reaches its descendants:

    exarch-agents `cancel #'plan-review'#

Look up provider/model pairs before choosing another selection. Fuzzy-search both names, or list the full catalog:

    exarch-agents `grep-models #'claude'#
    exarch-agents `models

For permissions, selection syntax, fan-out, and result schemas, consult the command guide:

    explain exarch-agents
