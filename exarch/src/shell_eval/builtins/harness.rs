//! The harness builtins — `exarch-agents`, `exarch-schedules`, `exarch-pins`,
//! `exarch-context`, `exarch-transcript` — with the type schemes that gate
//! them. A returning agent's reply is a tag of `exarch-agents`
//! (`` `reply ``), not a builtin of its own — the fleet is one family.
//!
//! Each builtin reads the model's argument as a [`Request`] before it enquires,
//! so a malformed call never reaches the host, and is refused in the words
//! the desk would use. `exarch-agents`'s `` `start `` tag forks this shell and
//! tells the host how to reach the fork, which is what the run's [`Fork`]
//! says: an in-process host adopts a bequest parked in the run's nursery,
//! since the reentrancy law bars a desk handler from holding `&mut Shell` to
//! fork one itself; a host across a wire is named a sealed bequest, and builds
//! an engine around it while it answers. [`crate::agent::desk::ExarchDesk`]
//! answers every enquiry on the other side.
//!
//! All but `exarch-transcript` name a state, and answer it afterwards.
//! `exarch-transcript` names the record instead, which no tag of it writes,
//! so its tags answer what they were asked for rather than a state.

use crate::enquiry::{
    Agents, Context, Evict, Family, ForkClaim, Launch, Pins, Reading, Request, Schedules, Start,
    Survey, Transcript, Turns, Word, family,
};
use crate::schedule::ScheduleInfo;
use ral_core::data::FOValue;
use ral_core::data::datum::Datum;
use ral_core::shell::runtime::{BuiltinBody, BuiltinEntry, Fork, Mooring, Settled, sig};
use ral_core::ty::Site;
use ral_core::ty::{Kind, Row, RowVar, Scheme, Ty, TyVar, Typed as _, closed_record, open_variant};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{fun, mk_scheme as scheme, pure, thunk};
use ral_core::{Shell, SpawnGrant, Value};
use std::borrow::Cow;
use std::sync::Arc;

const AGENTS: &str = "exarch-agents";

/// The model's argument as first-order data, the only kind that crosses.
fn first_order(verb: &str, arg: &Value) -> Settled<FOValue> {
    FOValue::try_from(arg).map_err(|_| {
        sig(format!(
            "{verb}: the argument must be first-order data; no closures, handles, or \
             environments: since it crosses to the host as plain data"
        ))
    })
}

/// Enquire `request`, admitting the host's answer only in the shape owed.
fn ask(verb: &str, request: Request, mooring: &Mooring, shell: &Shell) -> Settled<Value> {
    let owed = request.owed();
    let answer = shell.enquire(mooring, request.encode())?;
    owed(&answer).map_err(|why| sig(format!("{verb}: the host answered out of shape; {why}")))?;
    Ok(Value::from(answer))
}

/// `exarch-<class>` for every family but the fleet: one enquiry per call.
fn builtin_family<F: Family>(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let verb = format!("exarch-{}", F::CLASS);
    let request = family::<F>(&first_order(&verb, &args[0])?).map_err(sig)?;
    ask(&verb, request.request(), mooring, shell)
}

/// A host's answer let into the program: a family whose answer is not one fixed
/// shape leaves its type to the script, and the site says what the script made
/// of it.
fn admitted(verb: &str, site: &Site, answer: Value, shell: &Shell) -> Settled<Value> {
    site.admit(&answer, shell.fixings())
        .map_err(|mismatch| mismatch.refusal(verb, shell))?;
    Ok(answer)
}

/// [`builtin_family`] for a family whose answer is the script's to decide.
fn boundary_family<F: Family>(
    args: &[Value],
    site: &Arc<Site>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let answer = builtin_family::<F>(args, mooring, shell)?;
    admitted(&format!("exarch-{}", F::CLASS), site, answer, shell)
}

/// `exarch-agents <tag>`: `` `start `` forks before it enquires, every other
/// tag is one enquiry.
fn builtin_agents(
    args: &[Value],
    site: &Arc<Site>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let answer = agents(args, mooring, shell)?;
    admitted(AGENTS, site, answer, shell)
}

fn agents(args: &[Value], mooring: &Mooring, shell: &Shell) -> Settled<Value> {
    match Word::decode(&first_order(AGENTS, &args[0])?).map_err(sig)? {
        Word::Ask(request) => ask(AGENTS, Request::Agents(request), mooring, shell),
        Word::Spawn(Launch { grant, spec }) => fork_then_enquire(grant.0, mooring, shell, |fork| {
            Request::Agents(Agents::Start(Start { spec, fork }))
        }),
    }
}

/// Fork this shell and enquire `request` of the fork's receipt, through the
/// route this run's [`Fork`] names: parked in process, or sealed across a
/// wire.
fn fork_then_enquire(
    grant: SpawnGrant,
    mooring: &Mooring,
    shell: &Shell,
    request: impl FnOnce(ForkClaim) -> Request,
) -> Settled<Value> {
    let claim = match mooring.fork() {
        Some(Fork::Park(nursery)) => ForkClaim::Parked(nursery.park(shell.bequeath(grant))),
        Some(Fork::Seal) => ForkClaim::Sealed(
            ral_core::seed::seal(&shell.bequeath(grant))
                .map_err(|why| sig(format!("{AGENTS}: {why}")))?,
        ),
        None => {
            return Err(sig(format!(
                "{AGENTS}: this host adopts no forked sessions"
            )));
        }
    };
    ask(AGENTS, request(claim), mooring, shell)
}

/// A family builtin's scheme.  `first_order` refuses a block or handle at any
/// depth of the argument, so the type variables the argument mentions are
/// data and its row variables deep; `answers` are the result's, which the
/// boundary admits against what it sent.
fn family_scheme(data: &[TyVar], answers: &[TyVar], rows: &[RowVar], ty: Ty) -> Scheme {
    let vars: Vec<_> = data
        .iter()
        .map(|&v| (v, Kind::DATA))
        .chain(answers.iter().map(|&v| (v, Kind::ANY)))
        .collect();
    let rows: Vec<_> = rows.iter().map(|&v| (v, true)).collect();
    scheme(&vars, &rows, ty)
}

/// `exarch-agents :: ∀β:data α ρ1^d ρ2^d ρ3^d ρ4^d. <list | start [prompt: Str, name: Str, type: Variant ρ1, grant: Variant ρ2, model: Variant ρ3] | message [to: Str, text: Str] | stop Str | dismiss Str | reply β | read Str | ρ4> → F α`
///
/// The outer tag row is open (`ρ4`) so an unrecognised tag reaches the
/// runtime decoder that names the legal ones, rather than dying as a
/// row-unification mismatch.
///
/// The answer is not one fixed shape: `` `list `` answers the roster
/// `[[name, spawner, state, idle-s, elapsed-s, log-dir]]`, the acting tags the
/// summary `[live, replied]`,
/// and `` `read `` the value a
/// descendant handed up, whose shape this call cannot know — so `α` is left
/// free rather than fixed to one of these. This is the
/// `` `exarch-pins `read `` / `from-json` move ([`scheme_pins`]): the boundary
/// admits whichever shape [`Request::owed`] names for the tag it sent.
///
/// `start`'s and `message`'s record rows are closed because a record
/// literal with literal keys infers an exact one (`infer_map_val` builds on
/// `Row::Empty`), so a missing or misspelled field is a static error naming
/// it. The `type`, `grant` and `model` rows *inside* `start` stay
/// open, because a literal tag infers its own open row: closing them would
/// make `` `bogus `` a bare row-mismatch diagnostic that never reaches the
/// vocabulary's decoders, which enumerate the legal labels.
/// `reply`'s `β` is likewise trusted first-order data, checked at the boundary
/// rather than by the row.
fn scheme_agents(u: &mut Unifier) -> Scheme {
    let type_row = u.fresh_row_var();
    let grant_row = u.fresh_row_var();
    let model_row = u.fresh_row_var();
    let tag_row = u.fresh_row_var();
    let reply_ty = u.fresh_tyvar();
    let answer_ty = u.fresh_tyvar();
    family_scheme(
        &[reply_ty],
        &[answer_ty],
        &[type_row, grant_row, model_row, tag_row],
        thunk(fun(
            open_variant(
                &[
                    ("list", Ty::Unit),
                    (
                        "start",
                        closed_record(&[
                            ("prompt", Ty::String),
                            ("name", Ty::String),
                            ("type", Ty::Variant(Row::Var(type_row))),
                            ("grant", Ty::Variant(Row::Var(grant_row))),
                            ("model", Ty::Variant(Row::Var(model_row))),
                        ]),
                    ),
                    (
                        "message",
                        closed_record(&[("to", Ty::String), ("text", Ty::String)]),
                    ),
                    ("stop", Ty::String),
                    ("dismiss", Ty::String),
                    ("reply", Ty::Var(reply_ty)),
                    ("read", Ty::String),
                ],
                tag_row,
            ),
            pure(Ty::Var(answer_ty)),
        )),
    )
}

/// `exarch-schedules :: ∀ρ1 ρ2. <list | add [trigger: Variant ρ1, label: Str, prompt: Str] | remove Str | ρ2> → F [[label: Str, trigger: Str, next-s: Int, fires: Int]]`
///
/// Same shape as [`scheme_agents`]: an open outer tag row so an unknown tag
/// reaches the decoder naming the three legal ones, a closed `add` record row
/// so a missing or misspelled field is static, and an open `trigger` row
/// inside it so an unrecognised trigger reaches the decoder, which names the
/// legal shapes. `label` is a plain `Str` — every schedule names
/// itself, so there is no shape left to leave open.
fn scheme_schedules(u: &mut Unifier) -> Scheme {
    let trigger_row = u.fresh_row_var();
    let tag_row = u.fresh_row_var();
    family_scheme(
        &[],
        &[],
        &[trigger_row, tag_row],
        thunk(fun(
            open_variant(
                &[
                    ("list", Ty::Unit),
                    (
                        "add",
                        closed_record(&[
                            ("trigger", Ty::Variant(Row::Var(trigger_row))),
                            ("label", Ty::String),
                            ("prompt", Ty::String),
                        ]),
                    ),
                    ("remove", Ty::String),
                ],
                tag_row,
            ),
            pure(Ty::list(ScheduleInfo::ty())),
        )),
    )
}

/// `exarch-pins :: ∀α ρ1^d ρ2^d. <set [key: Str, body: <| ρ1>] | clear Str | read Str | list | ρ2> → F α`
///
/// Same shape as [`scheme_agents`]: the outer tag row is open (`ρ2`) so an
/// unrecognised tag reaches the decoder, and `set`'s record row is closed since
/// a record literal infers an exact one. `body` is an open variant, the card
/// type `surface_op` takes, with a deep row: the label check stays
/// `Card::decode`'s, so one decoder owns it. `` `read ``'s
/// answer is `α`, the `from-json`/`` `read `` precedent
/// ([`ral_core::typecheck::builtins::scheme::from_json`]): trusted, not
/// checked, since only the desk's own decoder can judge whether the card
/// read back matches the shape it expects.
fn scheme_pins(u: &mut Unifier) -> Scheme {
    let card_row = u.fresh_row_var();
    let tag_row = u.fresh_row_var();
    let answer_ty = u.fresh_tyvar();
    family_scheme(
        &[],
        &[answer_ty],
        &[card_row, tag_row],
        thunk(fun(
            open_variant(
                &[
                    (
                        "set",
                        closed_record(&[
                            ("key", Ty::String),
                            ("body", Ty::Variant(Row::Var(card_row))),
                        ]),
                    ),
                    ("clear", Ty::String),
                    ("read", Ty::String),
                    ("list", Ty::Unit),
                ],
                tag_row,
            ),
            pure(Ty::Var(answer_ty)),
        )),
    )
}

/// One item as both `` exarch-context `survey `` and `` exarch-transcript `index `` name
/// it; the index adds `location`, which a closed row of this shape cannot carry,
/// so `` `exarch-transcript ``'s own answer type is left free.
/// `exarch-context :: ∀ρ. <survey | evict [from: Int, to: Int, note: Str] | ρ> → F [rows: [[from: Int, to: Int, role: Str, kind: Str, label: Str, bytes: Int]], total-bytes: Int]`
///
/// Same shape as [`scheme_agents`] and [`scheme_schedules`]: an open outer
/// tag row so an unknown tag reaches the decoder naming the two legal ones.
///
/// `evict`'s record is closed, `note` included: a residue stands on its note.
/// The decoder refuses an empty, oversized, or multi-line one.
///
/// One answer for both tags: an edit changes what is addressable, so the
/// survey the transition leaves behind is what the next edit must be written
/// against.
fn scheme_context(u: &mut Unifier) -> Scheme {
    let tag_row = u.fresh_row_var();
    family_scheme(
        &[],
        &[],
        &[tag_row],
        thunk(fun(
            open_variant(&[("survey", Ty::Unit), ("evict", Evict::ty())], tag_row),
            pure(Survey::ty()),
        )),
    )
}

/// `exarch-transcript :: ∀α ρ. <index | read [turns: [Int]] | grep [pattern: Str, turns: <all | only [Int]>] | ρ> → F α`
///
/// The outer tag row is open (`ρ`) so an unrecognised tag reaches the
/// runtime decoder that names the three legal ones, rather than dying as a
/// row-unification mismatch.
///
/// Each tag answers its own shape — a listing, one record per turn a read
/// named, a hit table — so `α` is left free rather than fixed to any one of
/// them, exactly as [`scheme_agents`] leaves it free for `` `read ``. The
/// answer's shape is then [`Request::owed`]'s to check and the docstring's to
/// state.
///
/// `read`'s record is closed: its one field is the address it reads, so a
/// misspelling is a type error rather than a call that reaches the host
/// naming nothing. `grep`'s is closed too: `turns` is `` `all `` or `` `only [Int] ``, so
/// absence is data.
fn scheme_transcript(u: &mut Unifier) -> Scheme {
    let tag_row = u.fresh_row_var();
    let answer_ty = u.fresh_tyvar();
    family_scheme(
        &[],
        &[answer_ty],
        &[tag_row],
        thunk(fun(
            open_variant(
                &[
                    ("index", Ty::Unit),
                    ("read", Reading::ty()),
                    (
                        "grep",
                        closed_record(&[("pattern", Ty::String), ("turns", Turns::ty())]),
                    ),
                ],
                tag_row,
            ),
            pure(Ty::Var(answer_ty)),
        )),
    )
}

// A named array, not a promoted temporary: rustc refuses promotion once an
// entry carries `BuiltinEntry`'s interior-mutable arity cache.
static HARNESS_BUILTINS_ARR: [BuiltinEntry; 5] = [
    BuiltinEntry::boundary(
        Cow::Borrowed("exarch-agents"),
        scheme_agents,
        r"exarch-agents <tag>  — delegate work, collect replies, and follow up with live agents.

Start a child

    let plan = from-string < #'design.md'#
    exarch-agents `start [
      prompt: #'Review $plan against src/solver/. Reply [verdict: `ok|`revise, issues: [[file: Str, line: Int, why: Str]]].'#,
      name: #'plan-review'#, type: `amnemon, grant: `read-only,
      model: `inherit
    ]

All five fields are required. name must be unique among live agents, 1–24 ASCII
letters, digits, hyphens, or underscores. start returns immediately; a notice
arrives when the child replies or ends. Do not poll for completion.

Both types inherit a snapshot of your bindings, cwd, and environment, but not
live job handles. Files remain shared. Later bindings in either shell do not
update the other shell.

- `amnemon starts without your conversation. Pass the task and binding names.
- `mnemon inherits your conversation and its cache, then receives the new prompt.

Raw prompts keep references such as $plan literal. The child reads the binding;
there is no need to interpolate or copy its contents into the prompt. Bound
blocks are inherited too: define a helper once and ask the child to call it.

Choose permissions and a model

grant adds a restriction to your authority; it cannot grant anything you lack:

- `inherit keeps your authority.
- `confined permits working-tree and scratch I/O and local tooling, offline and without the GPU.
- `read-only permits project reads; writes are limited to scratch; no GPU.
- `edit-only permits project edits, without build tooling or the GPU.
- `reasonable permits everyday development tooling, the GPU included.
- `restrict R accepts a capability record as in grant [...] { ... }.

Set non-overlapping write grants for concurrent editors. For example, this
permits project reads and writes only under src/solver/, within your authority:

    grant: `restrict [fs: [read: [`cwd #'.'#], write: [`cwd #'src/solver/'#]]]

On Unix, preserve all parent-permitted reads while narrowing writes:

    grant: `restrict [fs: [read: [`absolute #'/'#], write: [`cwd #'src/solver/'#]]]

The child's system prompt automatically shows its grant, including inherited
restrictions. Messaging it does not change that grant.

Omitted axes inherit, so neither example narrows exec. Executables allowed by
name, path, or directory also become readable, subject to enclosing grants and
explicit denies; unrestricted exec alone implies no reads. A present fs table
allows only listed writes. Records may specify exec, fs, net, gpu, editor,
and shell; explain grant gives their shapes.

model is `inherit, your own model, or `role #'name'#, a model the human bound to that name at
launch. Your system prompt lists any roles.

Read a reply

    let review = exarch-agents `read #'plan-review'#
    $review[reply][issues]

Returns [name: Str, reply: Value]. The reply is checked against the fields and
types your script uses; a missing field is reported at read, with its path.
Only your own children may be read, including ones that have ended. A clear
forgets earlier children; a reply or Stop does not. A child still working,
or one that failed or stopped, is refused with the reason. Reading does not consume
the value: repeated reads return it until a follow-up sets the child to
work again.

Message a running or replied agent

    exarch-agents `message [
      to: #'plan-review'#, text: #'Check src/parser/ for the same issue. Return the same shape.'#
    ]

Steer running work with a correction or new information, or give a replied
agent a follow-up task. A message can reach any other live agent in your tree,
including a parent or sibling, while it works. A resting helper accepts only
its parent or the human. A missing recipient or your own name is refused.
A helper rests for five hours by default (--helper-idle sets the limit), then
ends if there is nothing it may take up. A follow-up resumes it; reading does
not extend its rest limit.

Reply to your caller

    exarch-agents `reply [verdict: `ok, issues: []]

Available to spawned and headless agents; interactive roots converse with the
user instead. Return first-order data: no blocks, handles, or environments.
Write reply last: the value is deposited after the enclosing ral batch drains.
Final prose alone does not deliver a reply. A child keeps its helpers and
rests once its pending work settles; a headless root finishes. Human dealings
with a helper are quiet: its parent sees its status but receives no notice.

Stop a descendant

    exarch-agents `stop #'plan-review'#

Cancels only its current exchange; its own helpers keep working. The agent
remains available for follow-up messages. An absent or resting name is a no-op;
a live agent outside your descendants is refused.

Dismiss one of your own helpers

    exarch-agents `dismiss #'plan-review'#

Ends and joins that helper and its descendants, freeing its live name. You may
dismiss only a helper you started, not a grandchild; its parent may dismiss it.

List live agents

    exarch-agents `list

Returns your whole tree, including you:

    [[name: Str, spawner: `root|`agent Str,
      state: `busy|`waiting-on-agents|`replied|`waiting,
      idle-s: Int, elapsed-s: Int, log-dir: Str]]

spawner identifies the parent, or `root. state distinguishes working, waiting
for descendants, parked with a reply, and parked without one. idle-s counts
seconds since parking, zero while busy; elapsed-s counts seconds since birth.
Use list to recover names or inspect the tree, not as a completion poll.

Every operation except list and read returns [live: Int, replied: Int]. live
counts other live agents in your tree; replied counts direct children holding
a reply. Reading does not decrement replied, so it is not an unread count.

Run independent tasks concurrently

    let jobs = [
      [name: #'notes-a'#, file: #'notes/a.md'#],
      [name: #'notes-b'#, file: #'notes/b.md'#]
    ]
    for $jobs { |job|
      let material = from-string < $job[file]
      exarch-agents `start [
        prompt: #'Summarise $material. Reply [topic: Str, claims: [Str]].'#,
        name: $job[name], type: `amnemon, grant: `confined,
        model: `inherit
      ]
    }

Each child receives material as bound at its own start. The prompt stays literal;
only the binding changes. After both completion notices, collect the replies:

    map { |job| exarch-agents `read $job[name] } $jobs",
        builtin_agents,
    ),
    BuiltinEntry::new(
        Cow::Borrowed("exarch-schedules"),
        scheme_schedules,
        "exarch-schedules <tag>  — your self-wakeups. Every tag returns the live table, oldest first: [[label: Str, trigger: Str, next-s: Int, fires: Int]]. Needs the self-wakeup grant (--allow-schedule). Foreground only: inside spawn { … } every tag errors.\n\nexarch-schedules `list  — read the table: `trigger` as its source text, `next-s` the seconds to the next fire, `fires` the count so far. A spent one-shot removes itself, so an `after label that has vanished has fired. Use it to recover labels after an eviction.\nexarch-schedules `add [trigger: `cron <Str>|`after <Str>, label: <Str>, prompt: <Str>]  — arm a wakeup. When it fires, `prompt` (an instruction, not code) is posted to your mailbox and reaches you at the next exchange boundary, or wakes you if you are at rest, as `[scheduled '<label>' · <trigger>] <prompt>`, re-engaging you with no human present. `cron '<expr>'` recurs on a classic five-field cron expression in local time, e.g. `cron '0 9 * * 1-5'`: minute hour day-of-month month day-of-week, each a comma list of `*`, N, a-b, or a step (`*/15`), names allowed for month and weekday (jan, mon). `after '<n><unit>'` fires once, unit s/m/h/d, e.g. `after '30m'`. A trigger that can never fire is refused. `label` is required and unique among live schedules. Check the new row's `next-s` to catch a cron that parsed but means something else. A fire is skipped while the previous one sits undrained. A conversing agent's wakeup at rest opens a new job. A returning agent waits while its schedules are armed, then clears them with its report; `/clear` drops them all.\nexarch-schedules `remove <label>  — disarm a wakeup. An absent label is not an error: a one-shot may already have fired.\n\nA tag that raises may still have landed; `list shows what is armed.",
        BuiltinBody::Static(builtin_family::<Schedules>),
    ),
    BuiltinEntry::boundary(
        Cow::Borrowed("exarch-pins"),
        scheme_pins,
        "exarch-pins <tag>  — your register: named slots of pinned state that outlive any one exchange. Foreground only: inside spawn { … } every tag errors.\n\nexarch-pins `set [key: <Str>, body: <card>]  — overwrite slot `key` with `body`, a `card [...] or one bare mark, e.g. `text [...].\nexarch-pins `clear <key>  — empty a slot; a no-op when it is already empty.\nexarch-pins `read <key>  — `some the card in slot `key`, or `none when it is empty.\nexarch-pins `list  — the occupied keys, as [String].",
        boundary_family::<Pins>,
    ),
    BuiltinEntry::new(
        Cow::Borrowed("exarch-context"),
        scheme_context,
        r"exarch-context <tag>  — inspect or evict what is sent on your next model request.

Eviction removes conversation from context while keeping it readable through
exarch-transcript. It preserves bindings, files, and running work. Call this
command from the foreground; inside spawn { … } every tag errors.

Turns and ids

A user turn contains a prompt (or an import's opening). An assistant turn contains
one assistant message, its tool results, and any steering before the next request.
Ids rise per lineage and are never reused; every tool result ends with TURN: <id>,
the assistant turn it closes. Use those ids, not positions in a list.

Eviction takes one contiguous run of turns (and of residues, the notes earlier
evictions left) and leaves one residue standing for the run, carrying your note.
from and to are turn ids, both inclusive: [from: 41, to: 60] names turns 41–60
and is the address of the residue it leaves. A run that reaches into a residue
must cover it whole. The same span over one residue rewrites its note. A prompt
may be evicted; say in the note what was asked.

Survey

    exarch-context `survey

The survey lists what is in your context now: one row per turn or residue, oldest
first, from == to for a turn, kind #'residue'# for a residue and its note as label:

    [rows: [[from: Int, to: Int, role: Str, kind: Str, label: Str, bytes: Int]], total-bytes: Int]

role is #'user'# or #'assistant'#; a residue's is #'user'#. kind is #'own'#,
#'import'# (imported by the harness), #'inherited'# (recorded by an ancestor
before your fork), or #'residue'#. A turn's label is its first 50 characters,
or its first tool call's description. bytes is the item's serialized size;
total-bytes measures the current conversation. These are byte counts, not
token counts.

Evict

    exarch-context `evict [from: 41, to: 43, note: #'Search ruled out the lexer; inspect parse_options next. No edits.'#]

All three fields are required. note is a nonempty, single-line note of at most
240 UTF-8 bytes; longer notes and notes containing a newline are refused. The
result is the survey after the edit, in the same shape as above.

Refused: an id never recorded, a turn rewound or cleared away, a run holding
the turn being written now, and a run that cuts a residue, reaching into it
without covering it whole.

Recovery and cost

An eviction that reports an error may already have landed. Survey before retrying
so you do not name turns that have already left. Read evicted material with:

    exarch-transcript `read [turns: !{range 41 44}]

Provider cache reuse is lost from the start of the evicted run onward. Batch cuts
of completed work; removing a just-failed turn affects only the recent end.
Near the context limit the harness warns you. When the context passes the
trigger, a reserve beyond the warning, a forced eviction takes the oldest turns
at a turn boundary, without a note. It may take your current prompt too, so
evict first under your own note if you want to leave your conclusion, saying
what was asked.",
        BuiltinBody::Static(builtin_family::<Context>),
    ),
    BuiltinEntry::boundary(
        Cow::Borrowed("exarch-transcript"),
        scheme_transcript,
        r"exarch-transcript <tag>  — search and read recorded conversation, including evicted turns.

Use the same turn ids as exarch-context and the TURN: <id> tool-result footer.
The transcript includes your session and inherited history, including turns
withdrawn by /rewind or /clear. These operations only read; they do not restore
turns to their old positions in context. The current unclosed turn is unreadable.

Search by content

    exarch-transcript `grep [pattern: #'duplicate flag'#, turns: `all]
    exarch-transcript `grep [pattern: #'duplicate flag'#, turns: `only [31, 35]]

Both pattern and turns are required. pattern is a Rust regex; turns is `all or
`only a list of ids. grep searches turns' text and residues' notes: message
text, tool scripts, tool results, recorded reasoning, and every note a residue
keeps, which `only reaches when it names every id the residue covers. It returns:

    [hits: [[from: Int, to: Int, kind: Str, role: Str, line: Int, text: Str]], total: Int]

A hit names the item by from, to and kind: a turn's from == to, and a note's
kind is #'residue'#. role is the message's role as a string. line is 1-based
within that message's searched text, not a source-file line; a note is line 1.
There are at most 100 hits, oldest first, and each hit's text is clipped to
200 bytes. total counts all matches. If total exceeds the returned hit count,
narrow the pattern or turn ids: the returned hits may omit recent matches.

Find turns by label or size

    exarch-transcript `index

index lists every turn and every residue ever recorded, in id order, a residue
before what it covers:

    [[from: Int, to: Int, role: Str, kind: Str, label: Str, bytes: Int, location: `resident|`under [from: Int, to: Int]|`withdrawn]]

The first six fields have the same meaning as in exarch-context `survey.
location is `resident, `under [from, to] naming the innermost residue over it,
or `withdrawn. A turn's bytes is its own serialized size, wherever it lies. Use
the index when you know the stretch of work you want but do not have a useful
search term.

Read selected turns

    exarch-transcript `read [turns: [35]]

read [turns: [Int]] reads turn material; !{range a b} is half-open, so the turns
under [from: 41, to: 60] are !{range 41 61}. Returns one record per selected
turn, in id order:

    [[turn: Int, role: Str, messages: [Message]]]

read does not truncate the contents. Read a small turn directly; bind a large
one before inspecting the relevant messages or parts:

    let old-turn = exarch-transcript `read [turns: [35]]

A Message is [role: `system|`user|`assistant|`tool, parts: [Part]]. Its role is a
variant; the outer turn's role is a string. A Part is one of:

    `text [content: Str]
    `program [tool: Str, source: Str, keys: [Str]]
    `result [content: Str]
    `reasoning [content: Str]
    `binary [content-type: Str, name: Str, bytes: Int]
    `custom [provider: Str, model: Str]

A program holds the ral script in source; for another tool it gives the tool
name and argument names in keys. A result holds tool output. A binary part
contains media metadata only. Use case to select a part's payload; keys reads
maps, not variants. Search hits are previews: read their turn when you need the
complete diagnostic or script, and check current files before reusing old code.",
        boundary_family::<Transcript>,
    ),
];
pub static HARNESS_BUILTINS: &[BuiltinEntry] = &HARNESS_BUILTINS_ARR;

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
