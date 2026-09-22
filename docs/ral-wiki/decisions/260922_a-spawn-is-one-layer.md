---
status: active
generated_at_commit: 451d1ab5
---

# A spawn is one layer, so `grant` is one field

**At the root, `--base` and `--restrict` are two different acts; at a spawn they
are the same act — pushing one layer onto a stack that is already there — so a
child's authority is stated by *one* field with six spellings, not by two
fields.** The spawn spec's `grant` widens to

```text
  grant: `inherit | `confined | `read-only | `edit-only | `reasonable | `restrict R
```

where `R` is a capability record, written in the parent's shell, in exactly the
vocabulary `grant [...] { body }` and every bake-in profile already speak.

## Why one field and not two

`--base` at the CLI *establishes* a ceiling: there is nothing beneath it, so the
profile it names is the whole of the session's authority, and naming a looser
one genuinely grants more. `--restrict` *narrows* what is already established,
by pushing a layer the stack's per-check fold ANDs with the rest
([[map/exarch/policy|policy]]). Two acts, because at the root they have
different effects.

At a spawn they do not. `fork_child` (`exarch/src/fleet/desk.rs`) clones the
parent's own `GrantStack` and pushes the named base onto the clone — a base
pushed *as if it were a restrict*, with the parent's layers underneath it, so it
can only narrow. `base_layer` already says this in its own doc comment: "the
stack is the meet, so a spawn narrowing a child only ever adds a layer; naming a
base looser than the parent changes nothing once folded." There is therefore
exactly one question at a spawn — *what single layer does this child get?* — and
a surface with two fields for it would be two spellings of one answer, with a
rule needed for what it means to write both.

So the spawn's vocabulary widens instead: a bake-in name is one way of saying
what that layer is, a literal record is another. The CLI is deliberately **not**
renamed to match. Its two flags are not a redundancy to be tidied away — they
are two acts, and the mirror worth having between the CLI and the spawn is in
the vocabulary of *values*, not in the vocabulary of flags.

## `` `dangerous `` leaves the spawn surface

`` `dangerous `` resolved to `Capabilities::root`, the lattice top: a layer that
says nothing, which ANDed with the parent's stack leaves the parent's authority
exactly as it stands. What it *meant* at a spawn was therefore never "dangerous"
— a `confined` parent spawning a `` `dangerous `` child got a confined child.
The tag named the root's reading of ⊤ and imported it into a place where ⊤ has
the opposite sense.

`` `inherit `` says the true thing, and says it in the spelling the sibling
fields already use: `provider` and `model` both write `` `inherit `` for *I have
no opinion* ([[map/exarch/builtins|builtins]]). One word now means one thing
across the spec record.

`` `dangerous `` stays a `--base` name, unchanged. At the root ⊤ is no-ceiling,
which is genuinely dangerous and is what a human asking for it is asking for.

## `R` is decoded, not typed

The `grant` row in `scheme_agents` stays **open**, under the house law an open
row already carries: an open row means a runtime door that enumerates the legal
labels in its own message
([[decisions/260719_agent-names-and-schedule-labels|names-and-schedule-labels]],
"closed rule, named labels"). So `R` is admitted at that door — which names all
six shapes and `R`'s own keys in its refusal, and holds it to first-order data,
since the ceiling crosses to the far side as data — and decoded, when the layer
is resolved, by `decode_capability_map`: the same function, walking the same
`Form::Grant` declared table, that `grant [...] { body }` and every
`*.exarch.ral` bake-in already pass through
([[map/core/capabilities|capabilities]]).

One table, one wording, no drift. A key `R` misspells is refused by the table's
own `unknown_key` sentence, which is the sentence the same mistake earns in a
`grant` block and in a `--restrict` file. Nothing is added to the type system:
giving `R` a static type would mean a second description of the capability
keyset, in a different language, kept in step by hand — which is the drift the
declared table exists to prevent.

## The wire carries the record unfrozen

`EngineSeed.grant` stops being a validated base tag and becomes
`ral_core::SpawnGrant { Inherit, Base(String), Restrict(FOValue) }`. The
`` `restrict `` arm crosses the wire as the **record**, undecoded.

That is not laziness about the wire format, it is where the freeze has to
happen. A restriction record's `cwd:` / `~` / `tempdir:` sigils must resolve
against the *child's* working directory, and for a wire spawn the only side
standing in it is the guest — so the decode, and with it the freeze, is
guest-side ([[decisions/260731_launch-cwd-is-the-freeze-anchor|launch-cwd-is-the-freeze-anchor]],
[[design/capability-freeze|capability-freeze]]). Sending a frozen
`Capabilities` would mean freezing against the host's idea of *here*; sending
the record preserves "a `Capabilities` has every path already resolved" as a
**construction** invariant, which is exactly what licenses that type's
unguarded `Serialize`.

Both seats then resolve a grant through one function, `SpawnGrant::layer`:
`` `inherit `` is no layer, `` `base `` reaches the host's own base lexicon
(core has none — `policy::base_layer` on the desk, the `EngineInstaller`'s
`GrantNarrower` in a guest), `` `restrict `` decodes the record against the
child's cwd. Where the desk and `apply_seed` each held their own narrowing
decision, there is now one.

## No self-denial for a spawn's `` `restrict ``

`--restrict` mints a deny layer over the restriction *files'* own paths, so an
agent cannot rewrite the bytes that shape its permissions
([[map/exarch/policy|policy]]). A spawn's `R` has no such bytes: it is a value
computed in the parent's shell at the moment of the spawn, and the child never
holds it. There is nothing to protect, so the asymmetry between the two
surfaces dissolves rather than needing a mirror on the spawn side.

The credential deny is untouched: it lives in `for_invocation`, beneath every
child, and a pushed layer can only narrow past it
([[decisions/260905_a-grant-does-not-hand-out-its-own-key|a-grant-does-not-hand-out-its-own-key]]).

## What does not change

- **Non-escalation stays structural.** A spawn contributes one more pushed
  layer and the per-check fold ANDs; an `R` that claims authority the parent
  does not hold is simply ANDed away, and needs no comparison against the
  parent to be safe.
- **The ceiling is still stated at the spawn site, and still mandatory.**
  `Avatar::fork_with` takes the child's authority as an argument rather than
  cloning the parent's ([[map/exarch/agent|agent]]).
- **The bundled-tools rule still bounds the bake-in names.** A base whose
  `exec` is directory prefixes alone leaves the child unable to run `ls` and
  with no way to ask for it back, which is why `minimal` is a `--base` name and
  not a spawn one ([[map/exarch/policy|policy]]). `` `restrict `` does not
  reopen that question: it narrows the parent, it does not replace a ceiling.

## See also

[[design/agents|agents]] §Permissions (the child's ceiling),
[[design/grant|grant]] (the lattice the fold runs in),
[[design/capability-freeze|capability-freeze]] (resolved-by-construction),
[[map/exarch/policy|policy]] (`base_layer`, the bake-ins, the deny layers),
[[map/exarch/builtins|builtins]] (the spawn record's door),
[[decisions/260906_object-not-name|object-not-name]] (why composition is
layering and there is no `Capabilities::meet`),
[[decisions/260719_agent-names-and-schedule-labels|names-and-schedule-labels]]
(the closed spec record with open variant rows).
