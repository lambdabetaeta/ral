`` agent [prompt: <Str>, name: <Str>, type: `amnemon|`mnemon, grant: <permission>, search: <Bool>] `` launches a sub-agent, asynchronously, immediately returning a receipt `[name: String, log-dir: String]`. A final reply from the sub-agent will arrive later. Give the child a descriptive kebab-case `name` ('todo-scan', 'fix-parser-tests'). `grant` is one of `` `confined ``, `` `minimal ``, `` `read-only ``, `` `edit-only ``, `` `reasonable ``, `` `dangerous `` and bounds the child to at most your own authority; `search` states whether it may use web search. `explain agent` has the full law.

Agents are expensive; use them sparingly, with the minimum grant that suffices. Here are four good reasons:

1. Explore: answer a question where you want the conclusion, not the working and context.
2. Isolate: perform actions whose execution would flood your context with detail you will not reuse.
3. Plan: survey the code with fresh eyes and return a detailed plan without the reasoning.
4. Verify: adversarially verify that a change is correct.

`` `amnemon `` agents are a fresh session. `` `mnemon `` agents are a fork of the current conversation, with `prompt` as the final prompt. Use `` `mnemon `` agents only if the current context contains significant information that the agent would have to rediscover. Otherwise, use the shell to transfer information to an `` `amnemon `` agent. 

A new agent's shell is an exact copy of yours, and preserves all bindings and the cwd. You may hand information to a subagent by binding it, and mentioning the variable name in the prompt: e.g. `let ctx = from-string < design.md` along with the prompt `#'Review the plan bound at $ctx against src/solver/; reply with a verdict record.'#` The same goes for scripts: any parameterized block is inherited, so define e.g. `let run-test-suite = { … }` and mention it in the prompt. NEVER paste into a prompt what a binding can carry. Warning: process handles cannot be copied, so `await` first if the child needs the result.

Poll live descendants with `agents` (returns `[[name, elapsed-s, log-dir]]`), communicate with them using `message <name> <text>`, and stop one with `agent-cancel <name>`.
