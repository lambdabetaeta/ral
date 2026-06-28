# Plan 0 — Source-only host turns

Implementation plan for **Phase 0** of
[[decisions/260628_host-seam-transport-parametric]]. Scope: remove the one host
construct that cannot survive a boundary — a live `Value` handed in as the program
of a turn — and replace it with a **named program table** dispatched by the engine
itself. No transport, no serialisation, no new IR. Everything below reuses
machinery that already exists: `parse` → `elaborate` → `typecheck` → `apply` /
`eval_top_level`, the `Env`, the handler stack, and the `pseudo_var` read path.

## The punchline

A "host program" is just a `Value::Block` / `Value::Lambda` we already build while
evaluating the rc file or a plugin file. **Registering** it = storing that value by
name in a session-lived table instead of in a user variable or a host-held field.
**Running** it = `apply(program, args)` inside the existing `run_built` scaffold
(`core/src/driver.rs:372`). A block's `body` is already an `Arc<Comp>` (the
compiled IR), so the program runs directly — there is no re-parsing and no
generated "wrapper" source.

The seam stops carrying a value; it carries a **name** the engine resolves against
its own table. The block never leaves the engine.

## Decisions locked in this plan

1. **`RAL_PROMPT`-the-variable dies.** The prompt is no longer a value looked up in
   the lexical scope (`ral/src/repl/prompt.rs:204`) and applied. It is a **declared
   program** registered by name. You change your prompt by re-declaring it, not by
   assigning a variable.
2. **The prompt is a zero-input program.** `USER` / `CWD` / `STATUS` are *not*
   passed in. They become **ambient reads** served by `pseudo_var`
   (`core/src/types/shell/mod.rs:428`), computed on read from engine state the
   engine already owns. `PromptBindings` (`prompt.rs:50–104`) is deleted outright,
   including the `set_var` / `set_env_var` side effects that today leak those three
   into the user's scope and into every child process's environment.
3. **Per-event hook input is deferred, per-site.** Whether a plugin hook's input
   (buffer text, cursor, keystroke) arrives as a dispatched argument or as an
   ambient read is decided per site, not here. Phase 0 routes hooks through the
   program table; it does not settle their input convention. See *Open questions*.
4. **Mobility enforcement is deferred.** In-process, a program's captured `Env` may
   hold anything. The check that a registered program captures only transportable
   state belongs to the phase that adds a real boundary. Phase 0 leaves a single
   marked seam (`HostProgram::validate`) where that check will live, and does
   nothing in it yet.

## What exists today (the machinery we reuse)

- **Compile pipeline.** `parse(&str) -> Vec<Stmt>` (`core/src/syntax/parser.rs:109`)
  → `elaborate(ast, bindings) -> Comp` → `typecheck(comp, schemes) -> Comp`. The
  turn path bundles these as `compile_turn(shell, src) -> (Arc<Comp>, bool)`
  (`core/src/turn.rs`).
- **The runnable form.** `Value::Block { body: Arc<Comp>, captured: Arc<Env> }`
  (`core/src/types/value.rs:171`) and `Value::Lambda { param, body, captured }`
  (`value.rs:164`). The `body` is the compiled IR; the `captured` env is the
  namespace the literal closed over at its declaration site.
- **The two evaluator entries.** `eval_top_level(&Arc<Comp>, &mut Shell)`
  (`core/src/evaluator.rs:104`) and `apply(&Value, &[Value], &mut Shell)`
  (`core/src/builtins.rs:736`), both returning `Settled<Value>`.
- **The framed scaffold.** `Shell::run_built(req, foreground, wall, single_command,
  src, body: FnOnce(&mut Shell) -> Settled<Value>) -> TurnReport`
  (`core/src/driver.rs:372`). Gives IO regime, terminal access, capture, wall, and
  lifecycle hooks to any body. `run_source_turn` and the doomed `run_value_turn`
  both call it.
- **Ambient reads.** `Shell::pseudo_var(name) -> Option<Value>` (`shell/mod.rs:428`)
  computes `$env` *on read* by merging `std::env::vars()` with
  `context.env_overrides`. Nothing is stored; this is exactly the mechanism for
  "value readable from source without being in any scope."
- **Engine state behind the three prompt values.** `Shell::cwd()`
  (`core/src/types/shell/cwd.rs:55`), `Shell::last_status()`
  (`core/src/types/shell/control.rs:34`), `crate::platform::user_name()`.
- **Session-lived dispatch tables.** The handler stack
  `shell.mobile.context.handlers` already lives session-long on `Mobile` and is the
  model for where the program table goes.

## New machinery (small, and beside what exists)

### 1. The program table

> **It is a lexical binding, not a handler.** A registered program is a name bound
> to a `Value::Block`/`Lambda` that *captures a lexical environment* — exactly what
> a session `Env` binding is, and exactly what `RAL_PROMPT` already is today. It is
> **not** a handler: handlers are dynamically scoped, resolved at command position,
> and compose by the grant/handler meet (`HandlerStack::lookup`, `value.rs:690`); a
> program has none of that. So we reuse the **lexical** representation —
> `Binding { value, scheme }` (`core/src/types/env.rs:44`) with the scheme inferred
> by the existing `bind_value` path — not `HandlerEntry`.

Three namespaces, kept distinct:

1. **user value/lexical namespace** (`Env`) — `$name`, lexical capture; a session
   binding holding a block is *also command-invokable* (`foo args`).
2. **handler namespace** (`HandlerStack`) — dynamic, command-dispatched, meet.
3. **host-program namespace** (new) — lexically-natured named definitions reachable
   only by the host at lifecycle moments.

A host program is a #1-natured thing (lexical capture, named definition, static —
not #2's dynamic dispatch). It lives in #3 rather than literally in the user's `Env`
for one reason: **hygiene**. In `Env` it would be readable as `$prompt`, invokable
as a `prompt` command, and overwritable by user code, and it could not carry the
`Plugin(id)` namespacing. A separate table keeps host entry points out of the user's
value/command namespace.

On `Context`, beside `scope` and `handlers`:

```rust
struct HostProgram {
    binding: Binding,        // reuse the lexical shape: { value: Block/Lambda, scheme }
    sig:     HookSig,        // engine-declared fixed-arity signature for this kind
    policy:  DefaultPolicy,  // terminal access, capture, turn budget
    origin:  Span,           // declaration site, for diagnostics
}

enum Namespace { Session, Plugin(PluginId) }
struct ProgramName { namespace: Namespace, name: String }

programs: HashMap<ProgramName, HostProgram>
```

**The invariant that makes #3 a separate namespace: a program is a turn root, never
a command.** It is invoked only by the host, as the root of a fresh turn, via
`apply`. It is never resolved by `$name` and never consulted at command position
(command dispatch reads only `handlers`; value reads only `Env`). So `__prompt__` is
not a user-invokable command and not a readable variable; a hook can never act as a
`CatchAll`. Programs are flat, stable session entries keyed by `ProgramName`, added
and removed by host lifecycle (plugin load/unload, prompt re-declaration) — not
pushed/popped within a turn, not part of the meet.

The table is session-lived (survives across turns, like `handlers`). It is **not**
part of the source type environment: Phase 0 dispatches programs by name from host
code, so references to them do not flow through `session_schemes()` / `seed_env`
(`core/src/typecheck.rs:56,80`). (Plugin-internal calls between a plugin's own
programs are ordinary source resolved at plugin-load time against the plugin
namespace, and are unaffected.)

### 2. Registration

```rust
impl Shell {
    fn register_program(&mut self, name: ProgramName, program: Value,
                        policy: DefaultPolicy, origin: Span) -> Result<(), RegisterError>;
}
```

- Assert `program` is a `Block` or `Lambda`; otherwise error with `origin`.
- Build the `Binding` with the **existing** lexical `bind_value` scheme inference —
  the same path a session `let` of a block uses, so schemes are computed identically.
  The only difference from an ordinary session binding is the destination: the
  private `programs` table, not the user's `Env` scope.
- **Typecheck the program against its hook kind's fixed signature `HookSig`** (see
  *§ Fixed-arity hook signatures*). This is the typed contract registration buys: a
  prompt program must be `() -> String`, a buffer-change program must match its
  declared record-in / record-out shape, or registration fails *at declaration time*
  with `origin`.
- `HostProgram::validate(&program)` — the marked seam for later mobility checks. A
  no-op in Phase 0.
- Insert into `programs`.

The value comes straight from the already-evaluated config/manifest `Map` — it was
compiled by `parse`/`elaborate`/`typecheck` as a normal part of evaluating the rc
or plugin file. No new compile path.

### 3. Dispatch

```rust
impl Shell {
    fn run_program(&mut self, name: ProgramName, args: Vec<Value>,
                   req: TurnRequest) -> TurnReport;
}
```

1. Look up the `HostProgram` (error → `TurnReport` carrying a host diagnostic).
2. Assert every arg satisfies `is_ground` (see below).
3. `self.run_built(req, …, |s| crate::evaluator::apply(&prog.program, &args, s))`.

`run_program` *is* `run_value_turn` with two changes: the program is fetched from
the engine's own table rather than handed in, and its arguments are required to be
ground. The framing, IO, terminal lease, capture, and lifecycle are unchanged
because it goes through the same `run_built`.

### 4. The ground-value predicate

```rust
fn is_ground(v: &Value) -> bool   // unit, bool, int, string, bytes,
                                  // list/map/variant of ground; NOT Block/Lambda/handle
```

Used to guard `run_program` arguments. This is the only place Phase 0 asserts the
host conveys data, not closures, across the dispatch boundary.

## Ambient reads for the prompt

Add three derived entries to the read path so source can query engine state without
the prompt injecting it:

- In `Shell::pseudo_var` (`shell/mod.rs:428`), synthesise on read:
  - `CWD` from `self.cwd()`
  - `STATUS` from `self.last_status()`
  - `USER` from `platform::user_name()`

  Either fold them into the `$env` map computation (so `$env[CWD]` is live) or give
  them dedicated pseudo-vars (`$cwd`, `$?`, `$user`). Choose one; both reuse the
  existing `pseudo_var` dispatch.

This makes the three values readable by *any* program at *any* time, derived from
state the engine already maintains — not a per-cycle side effect of rendering the
prompt.

## Per-site lowering

| Site | Today | After |
| --- | --- | --- |
| **prompt** `prompt.rs:140` | `scope_lookup("RAL_PROMPT")` → `PromptBindings` env surgery → `run_value_turn(thunk, vec![], …)` | register the `prompt:` block as `Session/"prompt"`; each render `run_program(Session/"prompt", vec![], Denied + capture)`. Body reads `$env[CWD]` / `$?` / `$env[USER]`. |
| **startup** `boot.rs:332` | `run_value_turn(block, vec![], …)` | register the rc `startup:` block as `Session/"startup"`; `run_program(Session/"startup", vec![], Denied)` once. |
| **plugin factory** `load.rs:190` | `run_value_turn(factory, vec![options], …)` | register the factory as `Plugin(id)/"factory"`; `run_program(Plugin(id)/"factory", vec![options], Denied)` once. `options` must be ground. |
| **plugin hooks** `plugin.rs:401` | manifest stores handler *values*; `run_value_turn(hook, args, …)` | at load, `register_program(Plugin(id)/hook_name, handler, policy)` checking the handler against the kind's `HookSig`; manifest stores **names**. Event: front-end conveys one fixed ground **context record** as the argument; `run_program` returns one fixed ground **output record** — no mutable `PluginContext`. See *§ Fixed-arity hook signatures*. |
| **keybindings** `keybinding.rs:101` | as hooks, `Leased` | as hooks, fixed record in / record out, `Leased` policy. |
| **lifecycle** (pre/post/chpwd) `exec.rs:54,66,76` | direct `apply`, in-frame | manifest stores names; the in-frame call resolves the name and `apply`s it **inside the existing command frame**. Stays in-frame — no turn change, no `run_program`. |

For every program-valued site the value is a literal in the rc/plugin source, so it
is already a fully compiled `Block`/`Lambda` by the time the loader holds the
manifest `Map`. Registration grabs it; nothing is recompiled.

## Deletions

- `Shell::run_value_turn` (`core/src/driver.rs:349`) — the public door that accepts
  a host-held `Value` as a turn program. After the lowerings above, nothing calls
  it.
- `PromptBindings` and its `collect`/`apply`/`entries` (`ral/src/repl/prompt.rs:50–104`),
  the `scope_lookup("RAL_PROMPT")` in `render` (`prompt.rs:204`), the value branch
  in `eval_prompt` (`prompt.rs:109`), and the boot-time default-thunk install into
  the `RAL_PROMPT` variable (`ral/src/repl/session/boot.rs:161`).

`run_built` stays (private scaffold). `apply` and `eval_top_level` stay (private
evaluator entries). The public host evaluation surface becomes exactly:
`run_source_turn` (ad-hoc human/model source turns) + `run_program` +
`register_program`.

## Fixed-arity hook signatures

The two questions left open above — how hook input arrives, and how hook output
leaves — resolve together once the input is recognised as front-end state.

**Where the data lives decides ambient vs argument.** The editor's buffer, cursor,
keystroke, history, and keymap are all owned by the front-end (rustyline); the
engine holds *no* representation of the editing line — a turn only ever sees a line
as dispatched `src`. By contrast `cwd` / `last_status` / `user` are engine state.
So the rule is sharp:

> **ambient read ⇔ engine-owned standing state; argument ⇔ front-end-owned per-event
> state.**

The prompt's three are ambient (engine state). Every editor-hook input is an
argument the front-end conveys at dispatch. None can be an ambient read, because the
engine does not hold them.

**Arguments are fixed-arity, never variadic** (consistent with retiring
`ArgSig::Variadic`, commit `7f8f4d0`). Each hook kind has a *known, fixed* input
set, so each gets an engine-declared signature `HookSig` checked at registration. A
collection (history) is **one argument of list type**, not varargs. Because
`Value::Lambda` is unary (`param: IrPattern`), "fixed-arity" is realised as **one
fixed-shape ground record in, one fixed-shape ground record out**:

| hook | context record (in) | output record (out) |
| --- | --- | --- |
| **prompt program** | `{}` (zero input; reads ambient `cwd`/`status`/`user`) | `String` |
| **prompt hook** | `{ base: String }` | `String` |
| **buffer-change** | `{ old_buf, line: String, pos: Int, history: [String], keymap: String, state: S }` | `{ ghost: String?, highlights: [Span], state: S }` |
| **keybinding** | `{ line: String, cursor: Int, history: [String], keymap: String, state: S }` | `{ line: String, cursor: Int, accept: Bool, push: (String, Int)?, state: S }` |
| **lifecycle** (pre/post/chpwd) | `{ … per-event args }` | `Unit` (side-effecting, in-frame) |

This dissolves the mutable `PluginContext` (`plugin_editor.rs:149`): inputs are the
record argument, outputs are the record return. The one consequence to accept:

> **Plugin persistent state must be ground.** Today `state_cell` is an unconstrained
> `Option<Value>`. In the record model, prior state enters as the `state` field and
> new state leaves in the `state` field — so it must be ground (no closures/handles).
> This retires the `_ed-*` side-effecting builtins (`plugin_ed_builtins.rs`); a hook
> *returns* its state and outputs instead of mutating a shared context.

In-process this record threading is free; across a boundary it is exactly what
crosses the wire. Doing it in Phase 0 keeps the phase honest — a shared mutable
`PluginContext` is precisely the host↔program channel that cannot cross a seam.

## Suggested sequencing

1. Add the `programs` table to `Context` + `ProgramName` / `HostProgram` /
   `HookSig` / `DefaultPolicy`, with `register_program` (signature-checked) and the
   no-op `validate`.
2. Add `is_ground` and `run_program` over `run_built`.
3. Add the three ambient reads to `pseudo_var`.
4. Lower **prompt** (the motivating case): register + `run_program`, delete
   `PromptBindings` and the `RAL_PROMPT` variable path. Verify the default prompt
   and an rc-declared prompt render.
5. Lower **startup**, then **plugin factory**.
6. Lower **hooks** + **keybindings** to the fixed record-in / record-out signatures
   (*§ Fixed-arity hook signatures*): declare each `HookSig`, dissolve
   `PluginContext`, retire the `_ed-*` builtins, convert the manifest to store
   names; keep **lifecycle** hooks in-frame.
7. Delete `run_value_turn`. The build now proves nothing hands a host `Value`
   across the seam.

Each step is independently testable in-process, and each leaves the tree building.
