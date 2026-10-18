//! The harness builtins — `exarch-agents`, `exarch-schedules`, `exarch-pins`,
//! `exarch-context`, `exarch-transcript` — with the type schemes that gate
//! them. A returning agent's reply is a tag of `exarch-agents`
//! (`` `reply ``), not a builtin of its own — the fleet is one family.
//!
//! Each door reads the model's argument as a [`Request`] before it enquires,
//! so a malformed call never reaches the host, and is refused in the words
//! the desk would use. `exarch-agents`'s `` `start `` tag, and the host-only
//! `_exarch-branch`, fork this shell and tell the host how to reach the fork,
//! which is what the run's [`Fork`] door says: an in-process host adopts a
//! bequest parked in the run's nursery, since the reentrancy law bars a desk
//! handler from holding `&mut Shell` to fork one itself; a host across a wire
//! is named a sealed bequest, and builds an engine around it while it
//! answers. [`crate::agent::desk::ExarchDesk`] answers every enquiry on the
//! other side.
//!
//! All but `exarch-transcript` name a state, and answer it afterwards.
//! `exarch-transcript` names the record instead, which no tag of it writes,
//! so its tags answer what they were asked for rather than a state.

use crate::enquiry::{
    Agents, Context, Evict, Family, ForkClaim, Launch, Pins, Reading, Request, Schedules, Start,
    Survey, Transcript, Turns, Word, family,
};
use crate::schedule::ScheduleInfo;
use ral_core::first_order::FOValue;
use ral_core::first_order::datum::Datum;
use ral_core::ty::Site;
use ral_core::ty::{Kind, Row, RowVar, Scheme, Ty, TyVar, Typed as _, closed_record, open_variant};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{fun, mk_scheme as scheme, pure, thunk};
use ral_core::types::{BuiltinBody, BuiltinEntry, Fork, Mooring, Settled, sig};
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

/// `_exarch-branch`: the engine half of the host's `/branch`, forking this
/// session for the desk to adopt with the parent's whole authority.
fn builtin_branch(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    fork_then_enquire(SpawnGrant::Inherit, mooring, shell, |fork| {
        Request::Agents(Agents::Branch(fork))
    })
}

/// Fork this shell and enquire `request` of the fork's receipt, through the
/// door this run's [`Fork`] names: parked in process, or sealed across a
/// wire. `` `start `` and `/branch` differ only in the request.
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

fn scheme_branch(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], thunk(pure(Ty::Unit)))
}

/// A family door's scheme.  `first_order` refuses a block or handle at any
/// depth of the argument, so the type variables the argument mentions are
/// data and its row variables deep; `answers` are the result's, which the
/// door admits against what it sent.
fn door(data: &[TyVar], answers: &[TyVar], rows: &[RowVar], ty: Ty) -> Scheme {
    let vars: Vec<_> = data
        .iter()
        .map(|&v| (v, Kind::DATA))
        .chain(answers.iter().map(|&v| (v, Kind::ANY)))
        .collect();
    let rows: Vec<_> = rows.iter().map(|&v| (v, true)).collect();
    scheme(&vars, &rows, ty)
}

/// `exarch-agents :: ∀β:data α ρ1^d ρ2^d ρ3^d ρ4^d ρ5^d. <list | start [prompt: Str, name: Str, type: Variant ρ1, grant: Variant ρ2, provider: Variant ρ3, model: Variant ρ4] | message [to: Str, text: Str] | cancel Str | reply β | read Str | ρ5> → F α`
///
/// The outer tag row is open (`ρ5`) so an unrecognised tag reaches the
/// runtime door that names the six legal ones, rather than dying as a
/// row-unification mismatch.
///
/// The answer is not one fixed shape: `` `list `` answers the roster
/// `[[name, spawner, state, idle-s, elapsed-s, log-dir]]`, every other tag but
/// `` `read `` the summary `[live, replied]`, and `` `read `` the value a
/// descendant handed up, whose shape this call cannot know — so `α` is left
/// free rather than fixed to any of the three. This is the
/// `` `exarch-pins `read `` / `from-json` move ([`scheme_pins`]): the door
/// admits whichever shape [`Request::owed`] names for the tag it sent.
///
/// `start`'s and `message`'s record rows are closed because a record
/// literal with literal keys infers an exact one (`infer_map_val` builds on
/// `Row::Empty`), so a missing or misspelled field is a static error naming
/// it. The `type`, `grant`, `provider` and `model` rows *inside* `start` stay
/// open, because a literal tag infers its own open row: closing them would
/// make `` `bogus `` a bare row-mismatch diagnostic that never reaches the
/// vocabulary's decoders, which enumerate the legal labels.
/// `reply`'s `β` is likewise trusted first-order data, checked at the door
/// rather than by the row.
fn scheme_agents(u: &mut Unifier) -> Scheme {
    let type_row = u.fresh_row_var();
    let grant_row = u.fresh_row_var();
    let provider_row = u.fresh_row_var();
    let model_row = u.fresh_row_var();
    let tag_row = u.fresh_row_var();
    let reply_ty = u.fresh_tyvar();
    let answer_ty = u.fresh_tyvar();
    door(
        &[reply_ty],
        &[answer_ty],
        &[type_row, grant_row, provider_row, model_row, tag_row],
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
                            ("provider", Ty::Variant(Row::Var(provider_row))),
                            ("model", Ty::Variant(Row::Var(model_row))),
                        ]),
                    ),
                    (
                        "message",
                        closed_record(&[("to", Ty::String), ("text", Ty::String)]),
                    ),
                    ("cancel", Ty::String),
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
/// reaches the door naming the three legal ones, a closed `add` record row
/// so a missing or misspelled field is static, and an open `trigger` row
/// inside it so an unrecognised trigger reaches the door, which names the
/// legal shapes. `label` is a plain `Str` — every schedule names
/// itself, so there is no shape left to leave open.
fn scheme_schedules(u: &mut Unifier) -> Scheme {
    let trigger_row = u.fresh_row_var();
    let tag_row = u.fresh_row_var();
    door(
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
/// unrecognised tag reaches the door, and `set`'s record row is closed since
/// a record literal infers an exact one. `body` is an open variant, the card
/// type `surface_op` takes, with a deep row: the label check stays
/// `Card::decode`'s, so one door owns it. `` `read ``'s
/// answer is `α`, the `from-json`/`` `read `` precedent
/// ([`ral_core::typecheck::builtins::scheme::from_json`]): trusted, not
/// checked, since only the desk's own decoder can judge whether the card
/// read back matches the shape it expects.
fn scheme_pins(u: &mut Unifier) -> Scheme {
    let card_row = u.fresh_row_var();
    let tag_row = u.fresh_row_var();
    let answer_ty = u.fresh_tyvar();
    door(
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

/// One turn as both `` exarch-context `survey `` and `` exarch-transcript `index `` name
/// it; the index adds `held`, which a closed row of this shape cannot carry,
/// so `` `exarch-transcript ``'s own answer type is left free.
/// `exarch-context :: ∀ρ. <survey | evict [turns: [Int], note: <none | some Str>] | ρ> → F [rows: [[id: Int, role: Str, kind: Str, label: Str, bytes: Int]], total-bytes: Int]`
///
/// Same shape as [`scheme_agents`] and [`scheme_schedules`]: an open outer
/// tag row so an unknown tag reaches the door naming the two legal ones.
///
/// `evict`'s record is closed: `note` is `` `none `` or `` `some Str ``, so
/// absence is data rather than a missing key. The door refuses an empty,
/// oversized, or multi-line `` `some `` — a marker reading
/// `Your note at eviction: ""` is a defect.
///
/// One answer for both tags: an edit changes what is addressable, so the
/// survey the transition leaves behind is what the next edit must be written
/// against.
fn scheme_context(u: &mut Unifier) -> Scheme {
    let tag_row = u.fresh_row_var();
    door(
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
/// runtime door that names the three legal ones, rather than dying as a
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
    door(
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
static HARNESS_BUILTINS_ARR: [BuiltinEntry; 6] = [
    BuiltinEntry::boundary(
        Cow::Borrowed("exarch-agents"),
        scheme_agents,
        "exarch-agents <tag>  — your agents: start, message, and cancel descendants, and reply to whoever launched you.\n\nexarch-agents `list  — every extant agent, you included, with its `spawner` (or `root), `state`, and `idle-s` (seconds since it parked). `state` is `busy, `waiting-on-agents (held only by a descendant), `waiting (parked, no reply yet), or `replied.\nexarch-agents `start [prompt: <Str>, name: <Str>, type: `amnemon|`mnemon, grant: <permission>, provider: `inherit|`named <Str>, model: `inherit|`named <Str>]  — launch an agent; its reply arrives later as a notice. An `amnemon starts with an empty context, a `mnemon inherits yours and its cache. It shares your bindings, cwd, and env, but not live job handles. `name` is unique, non-empty ASCII, at most 24 characters. `grant` can only narrow your authority: `inherit, `confined (offline, no home reads), `read-only (writes only scratch), `edit-only (edits the cwd, no build tooling), `reasonable (everyday tooling), or `restrict <record>, shaped as for `grant [...]` (exec, fs, net, editor, shell). `provider` and `model` `inherit yours or name another; an unlisted model is refused.\nexarch-agents `message [to: <Str>, text: <Str>]  — deliver `text` to agent `to` as a marked item.\nexarch-agents `cancel <name>  — stop a descendant as soon as possible.\nexarch-agents `reply <value>  — hand `value` to whoever launched you; first-order data only (no blocks, handles, or environments).\nexarch-agents `read <name>  — what descendant `name` last replied, as [name: Str, reply: <value>].",
        builtin_agents,
    ),
    BuiltinEntry::new(
        Cow::Borrowed("exarch-schedules"),
        scheme_schedules,
        "exarch-schedules <tag>  — your self-wakeups. Every tag returns the live table, oldest first: [[label: Str, trigger: Str, next-s: Int, fires: Int]]. Needs the self-wakeup grant (--allow-schedule). Foreground only: inside spawn { … } every tag errors.\n\nexarch-schedules `list  — read the table: `trigger` as its source text, `next-s` the seconds to the next fire, `fires` the count so far. A spent one-shot removes itself, so an `after label that has vanished has fired. Use it to recover labels after an eviction.\nexarch-schedules `add [trigger: `cron <Str>|`after <Str>, label: <Str>, prompt: <Str>]  — arm a wakeup. When it fires, `prompt` (an instruction, not code) reaches your inbox at the next exchange boundary as `[scheduled '<label>' · <trigger>] <prompt>`, re-engaging you with no human present. `cron '<expr>'` recurs on a classic five-field cron expression in local time, e.g. `cron '0 9 * * 1-5'`: minute hour day-of-month month day-of-week, each a comma list of `*`, N, a-b, or a step (`*/15`), names allowed for month and weekday (jan, mon). `after '<n><unit>'` fires once, unit s/m/h/d, e.g. `after '30m'`. A trigger that can never fire is refused. `label` is required and unique among live schedules. Check the new row's `next-s` to catch a cron that parsed but means something else. A fire is skipped while the previous one sits undrained. While any schedule is live the session parks instead of ending, so an unremoved cron keeps you alive indefinitely; `/clear` drops them all.\nexarch-schedules `remove <label>  — disarm a wakeup. An absent label is not an error: a one-shot may already have fired.\n\nA tag that raises may still have landed; `list shows what is armed.",
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
        "exarch-context <tag>  — the turns your next request will send. A turn is a user turn (a prompt, or an import's opening) or an assistant turn (one assistant message, its tool results, and any steering before the next request). Ids rise per lineage and are never reused; every tool result ends with `TURN: <id>`, the assistant turn it closes. Every tag returns the survey: [rows: [[id: Int, role: Str, kind: Str, label: Str, bytes: Int]], total-bytes: Int]. Foreground only: inside spawn { … } every tag errors.\n\nexarch-context `survey  — read the survey, one row per turn, oldest first. `role` is `user` or `assistant`; `kind` is `own`, `import` (imported by the harness, e.g. on resume), or `inherited` (recorded by an ancestor before your fork); `label` is the turn's first 50 characters, or its first tool call's `description`; `bytes` is its serialised size. `total-bytes` is what is actually sent, markers included: weigh it against the provider's window.\nexarch-context `evict [turns: [Int], note: `none|`some Str]  — remove turns from the context, e.g. `turns: !{range 41 44}` for [41, 42, 43]. Refused: an unknown id, one already gone, or the turn this call belongs to. A user turn stays while any assistant turn answering it stays; a set emptied by this rule is refused. Each run of evicted turns leaves one marker listing them, with the note (one line, at most 240 bytes); `exarch-transcript `read` reads them back. Cost: the provider's cache is lost from the earliest evicted turn onward.\n\nNear the provider's window the harness evicts the oldest turns itself, without a note, after warning you once; evict first to attach one. A tag that raises may still have landed; `survey shows what is resident.",
        BuiltinBody::Static(builtin_family::<Context>),
    ),
    BuiltinEntry::boundary(
        Cow::Borrowed("exarch-transcript"),
        scheme_transcript,
        "exarch-transcript <tag>  — every turn of this session, evicted ones included, by the ids `exarch-context `survey` shows.\n\nexarch-transcript `index  — [[id: Int, role: Str, kind: Str, label: Str, bytes: Int, held: Str]], every turn in order: the survey's fields, and `held`, `resident` or `evicted`.\nexarch-transcript `read [turns: [Int]]  — [[turn: Int, role: Str, messages: [Message]]] in id order, each message exactly as sent. A Message is [role: `system|`user|`assistant|`tool, parts: [Part]]; a Part is `text [content: Str], `program [tool: Str, source: Str, keys: [Str]] (a tool call: the ral tool's script in `source`, any other tool's argument names in `keys`), `result [content: Str], `reasoning [content: Str], `binary [content-type: Str, name: Str, bytes: Int] (metadata only), or `custom [provider: Str, model: Str].\nexarch-transcript `grep [pattern: Str, turns: `all|`only [Int]]  — [hits: [[turn: Int, role: Str, line: Int, text: Str]], total: Int]: lines matching `pattern` (a Rust regex), at most the 100 oldest, `text` clipped to 200 bytes, `line` 1-based within its message; `total` counts them all.",
        boundary_family::<Transcript>,
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_exarch-branch"),
        scheme_branch,
        "_exarch-branch  — the host's `/branch`, internal: fork this session for the desk to adopt as a conversing branch.",
        BuiltinBody::Static(builtin_branch),
    ),
];
pub static HARNESS_BUILTINS: &[BuiltinEntry] = &HARNESS_BUILTINS_ARR;

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests;
