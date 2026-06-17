# Authority's three carriers, and why they don't collapse

**Authority changes its form as it travels — from a rule you write, to the act of
judging one specific action, to a flat checklist handed to a blunter guard — and
each step deliberately throws away something the next step must not be allowed to
see.** That is why there are three forms where one is tempting. They are not three
copies of one thing; they are authority at three stages, and the stages are not
interchangeable.

The seductive mistake is to think the combined permission is just a *smaller
rule*. Stack all the active rules, intersect them, and surely you get one tidy
rule you can check everything against? You don't — because whether a given action
is allowed depends on things no written rule can hold.

## `Capabilities` — the rule you write

This is permission as *language*: "allow `git`, but only `status` and `log`; let
me read my home directory but not write it." Two features make it a rule and not
yet a decision. It can be **silent** — say nothing about the network — so that
several rules can be layered and combined without each having to mention
everything. And it can be **composed**: stack two rules and the result is a third
rule of the same kind. It is the input — what an author writes and what
[[design/grant|`grant`]] layers narrow. Everything downstream is what becomes of
it.

## The live judgment — not a smaller rule

You might expect "combine the whole stack into one rule" to produce another
`Capabilities`. It can't, because the real question — *is this exact action
allowed, right now?* — turns on details a written rule does not contain:

- **the exact command** — "allow `git`, only `status`" cannot be boiled down to a
  list of files or programs; it depends on the literal words you typed.
- **who is asking, and from where** — the same rule means different concrete
  things checked inside the locked-down sandbox versus outside it.
- **where paths really point** — symlinks and `..` have to be resolved against
  the live environment before "inside my project" means anything.

So the combined form is not a tidier rule — it is *the act of judging*, and an act
is a verb, not a noun. It is realised as a function over the live `Context`: each
`capability::check_*(&Context, …)` folds the whole stack of rules and answers one
specific request as it arrives. The fold is the guarantee that a verdict was
weighed against *all* the rules at once, never one rule in isolation — held by
the function bodies, not by a witness type
([[decisions/260605_witness-collapse|witness-collapse]]).

## `SandboxProjection` — the checklist for a blunter guard

The same authority has to be enforced by two very different things, and they
understand different languages ([[design/two-enforcers|two-enforcers]]):

- **ral itself** is the sharp judge — it sees the exact command and arguments,
  knows where it is running, and rules on each action as it happens. So it never
  pre-decides; it judges live. That is the function over `Context`.
- **the operating system's sandbox** is the blunt but powerful guard — it can
  physically stop a runaway child program, but it is dim: you must hand it a flat
  list of "these locations yes, these no" *up front*, and it cannot ask
  follow-ups or understand "`git` but only `status`."

`SandboxProjection` is the permission boiled down into that flat list — the
subtle parts (which subcommand, this argument vs that) flattened away because the
guard could never act on them. Same authority, two renderings, because there are
two enforcers of unequal sophistication. Neither rendering serves both: hand the
flat list to the sharp judge and you have thrown away its precision; ask the
blunt guard to understand the subtle rule and it simply can't
([[decisions/260602_exec-authority-partitioned|the two folds stay separate]]).

## The shape of it

Picture a recipe, the cooking, and the photo on the menu. They concern one dish,
but you cannot substitute one for another: the recipe can be edited and combined
with others, the cooking is an act that needs the actual ingredients in front of
you, the photo is a flattened thing for someone who will never step into the
kitchen. Authority here is the same — *the written rule, the live judgment, the
exported checklist* — three forms, each forgetting something the next stage must
not recover. Merge them and you readmit exactly the confusions the separation
rules out.

See also [[design/grant|grant]], [[design/capability-freeze|capability-freeze]],
[[design/two-enforcers|two-enforcers]],
[[decisions/260605_witness-collapse|witness-collapse]],
[[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]];
realised in [[internals/capability-enforcement|capability-enforcement]]; map
[[map/core/capabilities|capabilities]].
