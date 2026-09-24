//! The harness builtins — `exarch-agents`, `exarch-schedules`, `exarch-pins`,
//! `exarch-context`, `exarch-transcript` — with the type schemes that gate
//! them. A returning agent's reply is a tag of `exarch-agents`
//! (`` `reply ``), not a builtin of its own — the fleet is one family.
//!
//! Each door reads the model's argument as a [`Request`] before it enquires,
//! so a malformed call never reaches the host, and is refused in the words
//! the desk would use. `exarch-agents`'s `` `start `` tag, and the host-only
//! `_exarch-branch`, fork this shell and tell the host how to reach the fork,
//! which is what the run's [`Fork`](ral_core::types::Fork) door says: an
//! in-process host adopts a fork parked in the run's nursery, since the
//! reentrancy law bars a desk handler from holding `&mut Shell` to fork one
//! itself; a host across a wire is handed a guest port to dial, and dials it
//! while it answers. [`crate::fleet::desk::ExarchDesk`] answers every enquiry
//! on the other side.
//!
//! All but `exarch-transcript` name a state, and answer it afterwards.
//! `exarch-transcript` names the record instead, which no tag of it writes,
//! so its tags answer what they were asked for rather than a state.

use crate::fleet::enquiry::{
    Agents, Context, Family, ForkClaim, Pins, Request, Schedules, Start, Transcript, Word, family,
};
use ral_core::serial::FOValue;
use ral_core::serial::datum::Datum;
use ral_core::typecheck::builtins::{
    closed_record, fun, mk_scheme as scheme, open_record, open_variant, pure, thunk,
};
use ral_core::typecheck::{Row, Scheme, Ty, Unifier};
use ral_core::types::{BuiltinBody, BuiltinEntry, Fork, Mooring, Settled, sig};
use ral_core::{Shell, SpawnGrant, Value};
use std::borrow::Cow;

const AGENTS: &str = "exarch-agents";

/// The model's argument as first-order data, the only kind that crosses.
fn first_order(verb: &str, arg: &Value) -> Settled<FOValue> {
    FOValue::try_from(arg).map_err(|_| {
        sig(format!(
            "{verb}: the argument must be first-order data — no closures, handles, or \
             environments — since it crosses to the host as plain data"
        ))
    })
}

/// Enquire `request`, admitting the host's answer only in the shape owed.
fn ask(verb: &str, request: Request, mooring: &Mooring, shell: &Shell) -> Settled<Value> {
    let owed = request.owed();
    let answer = shell.enquire(mooring, request.encode())?;
    owed(&answer).map_err(|why| sig(format!("{verb}: the host answered out of shape — {why}")))?;
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

/// `exarch-agents <tag>`: `` `start `` forks before it enquires, every other
/// tag is one enquiry.
fn builtin_agents(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    match Word::decode(&first_order(AGENTS, &args[0])?).map_err(sig)? {
        Word::Ask(request) => ask(AGENTS, Request::Agents(request), mooring, shell),
        Word::Spawn(spec) => {
            let grant = spec.grant.0.clone();
            fork_then_enquire(grant, mooring, shell, |fork| {
                Request::Agents(Agents::Start(Start { spec, fork }))
            })
        }
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
/// door this run's [`Fork`] names: parked in process, or listening across a
/// wire. `` `start `` and `/branch` differ only in the request.
fn fork_then_enquire(
    grant: SpawnGrant,
    mooring: &Mooring,
    shell: &Shell,
    request: impl FnOnce(ForkClaim) -> Request,
) -> Settled<Value> {
    let session = match mooring.fork() {
        Some(Fork::Listen) => return hatch_over_the_wire(grant, mooring, shell, request),
        // Dressed with the recipe's ledgers, as a hatched child's recipe
        // dresses it: a fork is scope and context, never session policy.
        Some(Fork::Park(nursery)) => {
            let mut fork = shell.fork_scrubbed();
            crate::bootstrap::arm_session_ledgers(&mut fork);
            nursery.park(fork)
        }
        // Core owns the sentence for a host that adopts no fork.
        None => shell.fork_into_nursery(mooring)?,
    };
    ask(AGENTS, request(ForkClaim::Parked(session)), mooring, shell)
}

/// Eight bytes the host must write before this engine hatches onto the
/// connection it dialled. The guest kernel's refusal to route a guest-local
/// dial is the standing defence; this is the second line, against a jailed
/// command that guesses a CID rather than reading one.
#[cfg(target_os = "linux")]
fn mint_token() -> u64 {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("OS randomness");
    u64::from_le_bytes(bytes)
}

/// The wire arm: bind a guest port for the duration of this one fork, name
/// it in the enquiry, and let the host dial while it answers.
///
/// The answer arrives only once the child exists, because the listener thread
/// acknowledges the dial after `spawn()` succeeds. So there is one enquiry and
/// one rule for its outcome: raise the thread's reason if it has one — it was
/// nearer the failure — otherwise the host's.
#[cfg(target_os = "linux")]
fn hatch_over_the_wire(
    grant: SpawnGrant,
    mooring: &Mooring,
    shell: &Shell,
    request: impl FnOnce(ForkClaim) -> Request,
) -> Settled<Value> {
    let token = mint_token();
    let (socket, port) =
        super::guest_port::bind().map_err(|why| sig(format!("{AGENTS}: {why}")))?;
    let listener = ral_core::hatch::listen_for_hatch(socket, token, shell, grant)
        .map_err(|reason| sig(format!("{AGENTS}: {reason}")))?;
    let answer = ask(
        AGENTS,
        request(ForkClaim::Listening { port, token }),
        mooring,
        shell,
    );
    // A host that refused never dialled, so the thread is still in its poll:
    // wake it, or the join below never returns.
    if answer.is_err() {
        listener.cancel();
    }
    match listener.join() {
        Err(ral_core::hatch::Unhatched::Failed(reason)) => Err(sig(format!("{AGENTS}: {reason}"))),
        _ => answer,
    }
}

/// The dial this arm waits for means nothing outside a Linux guest, so a wire
/// trunk built on any other platform refuses here rather than at a silent
/// no-op.
#[cfg(not(target_os = "linux"))]
fn hatch_over_the_wire(
    _grant: SpawnGrant,
    _mooring: &Mooring,
    _shell: &Shell,
    _request: impl FnOnce(ForkClaim) -> Request,
) -> Settled<Value> {
    Err(sig(format!(
        "{AGENTS}: this engine has no hatch support outside a Linux guest — a wire trunk's \
         helper spawn only ever reaches one"
    )))
}

fn scheme_branch(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(pure(Ty::Unit)))
}

/// `exarch-agents :: ∀α β ρ1 ρ2 ρ3 ρ4 ρ5. <list | start [prompt: Str, name: Str, type: Variant ρ1, grant: Variant ρ2, search: Bool, provider: Variant ρ3, model: Variant ρ4] | message [to: Str, text: Str] | cancel Str | reply β | read Str | ρ5> → F α`
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
/// vocabulary's decoders, which enumerate the legal labels. `search` is
/// two-state rather than an enumeration, so `Ty::Bool` closes it outright.
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
    scheme(
        &[reply_ty, answer_ty],
        &[],
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
                            ("search", Ty::Bool),
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

fn schedule_row_ty() -> Ty {
    closed_record(&[
        ("label", Ty::String),
        ("trigger", Ty::String),
        ("next-s", Ty::Int),
        ("fires", Ty::Int),
    ])
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
    scheme(
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
            pure(Ty::List(Box::new(schedule_row_ty()))),
        )),
    )
}

/// `exarch-pins :: ∀β α ρ1 ρ2. <set [key: Str, body: β] | clear Str | read Str | list | ρ2> → F α`
///
/// Same shape as [`scheme_agents`]: the outer tag row is open (`ρ2`) so an
/// unrecognised tag reaches the door, and `set`'s record row is closed since
/// a record literal infers an exact one. `body`'s `β` is left to the door,
/// which judges whether it is a card. `` `read ``'s
/// answer is `α`, the `from-json`/`` `read `` precedent
/// ([`ral_core::typecheck::builtins::scheme::from_json`]): trusted, not
/// checked, since only the desk's own decoder can judge whether the card
/// read back matches the shape it expects.
fn scheme_pins(u: &mut Unifier) -> Scheme {
    let body_ty = u.fresh_tyvar();
    let tag_row = u.fresh_row_var();
    let answer_ty = u.fresh_tyvar();
    scheme(
        &[body_ty, answer_ty],
        &[],
        &[tag_row],
        thunk(fun(
            open_variant(
                &[
                    (
                        "set",
                        closed_record(&[("key", Ty::String), ("body", Ty::Var(body_ty))]),
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
fn context_turn_ty() -> Ty {
    closed_record(&[
        ("id", Ty::Int),
        ("role", Ty::String),
        ("kind", Ty::String),
        ("label", Ty::String),
        ("bytes", Ty::Int),
    ])
}

fn context_receipt_ty() -> Ty {
    closed_record(&[
        ("rows", Ty::List(Box::new(context_turn_ty()))),
        ("total-bytes", Ty::Int),
    ])
}

/// `exarch-context :: ∀ρ1 ρ2. <survey | evict [turns: [Int] | ρ1] | ρ2> → F [rows: [[id: Int, role: Str, kind: Str, label: Str, bytes: Int]], total-bytes: Int]`
///
/// Same shape as [`scheme_agents`] and [`scheme_schedules`]: an open outer
/// tag row so an unknown tag reaches the door naming the two legal ones.
///
/// `evict`'s record row anchors `turns` — the address is the edit, so there
/// is nothing optional about it — and stays open on `ρ1` because `note` is
/// optional and a closed row cannot say so; the door refuses a `note` of the
/// wrong type, or an empty, oversized, or multi-line one — a required `note`
/// would invite `''`, and a marker
/// reading `Your note at eviction: ""` is a defect.
///
/// One answer for both tags: an edit changes what is addressable, so the
/// survey the transition leaves behind is what the next edit must be written
/// against.
fn scheme_context(u: &mut Unifier) -> Scheme {
    let evict_row = u.fresh_row_var();
    let tag_row = u.fresh_row_var();
    scheme(
        &[],
        &[],
        &[evict_row, tag_row],
        thunk(fun(
            open_variant(
                &[
                    ("survey", Ty::Unit),
                    (
                        "evict",
                        open_record(&[("turns", Ty::List(Box::new(Ty::Int)))], evict_row),
                    ),
                ],
                tag_row,
            ),
            pure(context_receipt_ty()),
        )),
    )
}

/// `exarch-transcript :: ∀α ρ2 ρ3. <index | read [turns: [Int]] | grep [pattern: Str | ρ2] | ρ3> → F α`
///
/// The outer tag row is open (`ρ3`) so an unrecognised tag reaches the
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
/// naming nothing. `grep`'s row is open on `ρ2`, since `turns` narrows it
/// and `pattern` alone is required, and a closed row cannot say so.
fn scheme_transcript(u: &mut Unifier) -> Scheme {
    let grep_row = u.fresh_row_var();
    let tag_row = u.fresh_row_var();
    let answer_ty = u.fresh_tyvar();
    scheme(
        &[answer_ty],
        &[],
        &[grep_row, tag_row],
        thunk(fun(
            open_variant(
                &[
                    ("index", Ty::Unit),
                    (
                        "read",
                        closed_record(&[("turns", Ty::List(Box::new(Ty::Int)))]),
                    ),
                    ("grep", open_record(&[("pattern", Ty::String)], grep_row)),
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
    BuiltinEntry::new(
        Cow::Borrowed("exarch-agents"),
        scheme_agents,
        "exarch-agents <tag>  — agent control: `list extant agents, `start a new one, `message them, `cancel them, `reply to your parent, `read the reply of a descendant.\n\nexarch-agents `list: every extant agent (including yourself), along with their `spawner` (or `root). `state` is `busy while working, `waiting-on-agents while held only by a descendant, `replied, or `waiting if it is parked but has not replied. `idle-s` is seconds since it parked.\n\nexarch-agents `start [prompt: <Str>, name: <Str>, type: `amnemon|`mnemon, grant: <permission>, search: <Bool>, provider: `inherit|`named <Str>, model: `inherit|`named <Str>]  — launch an agent, asynchronously; a reply will arrive later as a notice. An agent with an `amnemon` `type` starts with an empty context, while `mnemon` inherits yours (also reusing cache). Every new agent has the same bindings, cwd, and env as you, except live job handles. `prompt` is the agent's prompt, and can be computed as part of a ral script. `name` is the child's identity (non-empty, at most 24 characters, unique, ASCII only). `grant` controls permissions: `inherit (same as your authority), `confined (offline, no home reads), `read-only (may write to scratch), `edit-only (edits the cwd, no build tooling), `reasonable (everyday tooling), or `restrict <record>, which takes a capability record of the same shape as `grant [...]` (exec, fs, net, detach, editor, shell); note that this may only restrict authority, not expand it. `search` states whether the child may use web search. `provider` and `model` control the inference provider and model: `inherit share your own (default), and `named specify a different one.\n\nexarch-agents `message [to: <Str>, text: <Str>]  — send `text` as a marked item to the agent named `to`.\n\nexarch-agents `cancel <name> — stop a descendant agent as soon as possible.\n\nexarch-agents `reply <value>  — hand `value` back to your parent agent. `value` must be first-order data (no blocks, handles, or environments).\n\nexarch-agents `read <name>  — fetch the value the descendant named `name` last handed to `reply, as [name: Str, reply: <value>].",
        BuiltinBody::Static(builtin_agents),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("exarch-schedules"),
        scheme_schedules,
        "exarch-schedules <tag>  — your self-wakeups: `list what is armed, `add one, `remove one. Every tag answers with the table afterwards, [[label: Str, trigger: Str, next-s: Int, fires: Int]], so what you read back is always what is armed now rather than a receipt for what you just did. Requires the self-wakeup grant (--allow-schedule) — an agent that can wake itself indefinitely holds real authority, so without the grant every tag is refused.\n\nexarch-schedules `list  — your live wakeups, oldest first: label as you named it, trigger as its source text (a cron expression, or `after 30m`), next-s the seconds until the next fire, recomputed as you ask, and fires how many times it has fired so far. Only live schedules appear: a spent one-shot has already removed itself, so a label you armed with `after and then see no more of has fired, not vanished. This is how you recover labels after an eviction.\n\nexarch-schedules `add [trigger: `cron <Str>|`after <Str>, label: <Str>, prompt: <Str>]  — arm a self-wakeup: at the chosen time a marked item carrying `prompt` is delivered to your inbox and re-engages you with no human present. It drains at your next exchange boundary — as soon as the tool batch in flight settles, not only at the end of the exchange — and arrives as marked chrome, `[scheduled '<label>' · <trigger>] <prompt>`, never read as a command even when the prompt opens with `/`. `trigger` is exactly one of two variants; any other shape is refused, naming both. `cron '<expr>'` is recurring: five whitespace-separated fields, minute hour day-of-month month day-of-week, read in the host's local timezone — e.g. `cron '0 9 * * 1-5'` for weekdays at 09:00. Each field is a comma list of `*`, a number, a range `a-b`, or a step over either (`*/15`, `a-b/2`, `N/step` meaning N up to the field's maximum); month and day-of-week also accept three-letter names (jan…dec, sun…sat), and day-of-week accepts 7 as a second spelling of Sunday. When both day fields are restricted, either one matching fires it (Vixie-cron's OR rule); when only one is, that one decides. Every fire recomputes the next occurrence in the host timezone, so DST shifts, clock steps, and suspends are absorbed rather than accumulated. `after '<n><unit>'` is a one-shot relative delay from the moment of arming, unit one of s/m/h/d and the count greater than zero — e.g. `after '30m'`, `after '2h'`. A trigger with no next occurrence at all — a parseable but impossible date such as `cron '0 0 30 2 *'` — is refused here rather than arming silently. `label` names the wakeup and is its identity: it must not be borne by another live schedule, and you must always supply one. `prompt` is the natural-language instruction you act on when woken, not code. Read the new row's next-s out of the answer to catch a cron expression that parsed but does not mean what you meant. Once armed: an `after removes itself when it fires; a cron re-arms itself, and drops itself only when nothing further lies inside its search horizon. A fire whose previous wakeup is still sitting undrained in your inbox is skipped, not queued behind it, and does not count as a fire. While any schedule is live this session parks for the next wakeup at quiescence instead of ending, so a recurring schedule you never remove keeps this agent alive indefinitely — that is what the grant buys. `/clear` drops every live schedule.\n\nexarch-schedules `remove <label>  — disarm the wakeup bearing `label`; its next occurrence goes with it and nothing further is delivered. The entry is gone in the answer, so the row's absence is the confirmation. A label that was never there answers the same way, and that is no evidence of a mistake: a one-shot may have fired and removed itself since you read it.\n\nEach tag is one exchange with the host, and the table it answers is the schedule registry as it stands once the transition has landed. A raise still does not prove nothing happened: the transition may have landed and its answer failed to reach you. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_family::<Schedules>),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("exarch-pins"),
        scheme_pins,
        "exarch-pins <tag>  — your register of pinned state: a small set of named slots that outlive any one exchange. `set` writes a slot, `clear` empties one, `read` fetches a slot back, `list` names every occupied one. Reads and writes your own register only.\n\nexarch-pins `set [key: <Str>, body: <card>]  — overwrite the register slot named `key` with `body`, a `card [...]` value (or one of its marks bare, e.g. `text [...]`).\n\nexarch-pins `clear <key>  — empty the register slot named `key`. Clearing an already-empty slot is not an error.\n\nexarch-pins `read <key>  — the card currently pinned under `key`, as a `card value you can destructure, or () if the slot is empty.\n\nexarch-pins `list  — the keys currently occupied on your register, as [String]. Read one back with `read`.\n\nEach tag is one exchange with the host. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_family::<Pins>),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("exarch-context"),
        scheme_context,
        "exarch-context <tag>  — the context: the messages the provider is sent on your next request, as a list of turns. A turn is either a user turn — a prompt, or an import's opening — or an assistant turn — one assistant message, the tool results it called for, and any steering delivered before the next request. Turn ids are minted in one increasing sequence per lineage and never reused; every tool result ends with `TURN: <id>`, the id of the assistant turn it closes, and `exarch-context `survey` lists the rest. Every tag answers the survey after it has acted: [rows: [[id: Int, role: Str, kind: Str, label: Str, bytes: Int]], total-bytes: Int].\n\nexarch-context `survey  — acts on nothing. `rows` is one row per turn in the context, oldest first: `id` the turn's id; `role` `user` or `assistant`; `kind` `own` (recorded by this session), `import` (a note the harness imported, e.g. on resume), or `inherited` (recorded by an ancestor before you were forked); `label` the first 50 characters of the turn's first line, or of the `description` its first tool call declared where no prose opened one; `bytes` the serialised size of the turn's messages. `total-bytes` is the serialised size of what is actually sent — the resident turns plus every marker — and is the figure to weigh against the provider's context window.\n\nexarch-context `evict [turns: [Int], note: Str]  — removes the named turns from the context. `turns` is a list of turn ids in any order, repeats ignored; `!{range 41 44}` is [41, 42, 43]. Refused, naming the turn: an id never recorded; an id that has already left; the id of the turn being written now, i.e. the assistant turn whose result this call is part of. Kept silently: a user turn while any assistant turn answering it — the assistant turns between it and the next user turn — is in the context and not named; a set left empty by this rule is refused. Every other named turn leaves at once, wherever it lies. Where a run of consecutive turns has left, the context carries one marker in their place stating which turns left, one line per turn (id, role, label, KB; at most 40 lines per marker, older ones collapsed to a count), the note of the eviction that took them, and how to read them back. `note` is optional; if given it is one line of at most 240 bytes and appears verbatim in that marker. Evicted turns remain in the transcript and are readable with `exarch-transcript `read`. Cost: the provider's cache holds only the prefix before the earliest change, so the next request re-reads everything from the first evicted turn onward.\n\nWhen the context nears the provider's window, the harness evicts the oldest turns itself at the next turn boundary, without a note; as the context grows into the reserve before that point you are warned once, at a tool boundary, naming the turns the cut would take. Making that cut yourself is how a note gets attached.\n\nEach tag is one exchange with the host, and the survey it answers is the context as it stands once the transition has landed; an eviction lands at the desk immediately and is recorded. A raise still does not prove nothing happened: the transition may have landed and its answer failed to reach you. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_family::<Context>),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("exarch-transcript"),
        scheme_transcript,
        "exarch-transcript <tag>  — the complete record of this session, including turns that have been evicted from the context. Turns are specified as a list (e.g. `!{range 41 44}` or [41, 42, 43]). `exarch-context `survey` and `exarch-transcript `index` show the ids.\n\nexarch-transcript `index  — [[id: Int, role: Str, kind: Str, label: Str, bytes: Int, held: Str]], every recorded turn in order. The first five fields are the survey's; `held` is `resident` (in the context) or `evicted` (left it).\n\nexarch-transcript `read [turns: [Int]]  — [[turn: Int, role: Str, messages: [Message]]], one element per named turn in id order, each turn's messages exactly as the provider was sent them. A message is [role: `system|`user|`assistant|`tool, parts: [Part]]. A Part is one of: `text [content: Str]; `program [tool: Str, source: Str, keys: [Str]] — a tool call, where for the ral tool `source` is the script and `keys` is empty, and for any other tool `source` is empty and `keys` names its arguments; `result [content: Str] — a tool result as the model saw it; `reasoning [content: Str] — reasoning in full; `binary [content-type: Str, name: Str, bytes: Int] — a binary attachment's metadata (not the content); `custom [provider: Str, model: Str] — a provider extension's identity. \n\nexarch-transcript `grep [pattern: Str, turns: [Int]]  — [hits: [[turn: Int, role: Str, line: Int, text: Str]], total: Int]: every line of every message in the searched turns matching `pattern` (a Rust regex). `turns` is optional; absent, every recorded turn is searched. `hits` holds at most the 100 oldest matches, each `text` clipped to 200 bytes, `line` 1-based within its message; `total` is the count of all matches. `role` is the message's role.",
        BuiltinBody::Static(builtin_family::<Transcript>),
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
mod tests {
    use crate::agent::testkit::ral_call;

    /// Every bare tag the door admits: the bases, plus `` `inherit ``, which
    /// names none and so is the one admitted tag policy has nothing to resolve.
    fn bare_grant_tags() -> impl Iterator<Item = &'static str> {
        std::iter::once("inherit").chain(crate::policy::SPAWN_BASES)
    }

    /// The door validates `name`, `type`, and `grant` before
    /// `fork_into_nursery`/`enquire` ever run, so no child is registered.
    #[test]
    fn unknown_grant_label_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `bogus, search: true, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        for label in bare_grant_tags() {
            assert!(result.contains(label), "must name `{label}`, got: {result}");
        }
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "an unknown grant label must never register a child"
        );
    }

    /// `` `dangerous `` has left the spawn surface: there it resolved to ⊤, a
    /// layer saying nothing, which is what `` `inherit `` now says outright —
    /// so the refusal must send a model there rather than leave it guessing.
    #[test]
    fn dangerous_is_no_longer_a_spawn_grant_and_the_refusal_points_at_inherit() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `dangerous, search: true, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.contains("inherit is how you decline to narrow"),
            "the refusal must point at `inherit, got: {result}"
        );
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "`dangerous must never register a child"
        );
    }

    /// `` `restrict `` is the one grant tag that takes a payload, so bare it is
    /// a shape error, and the refusal must say what it was missing.
    #[test]
    fn a_bare_restrict_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `restrict, search: true, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.contains("capability record"),
            "the refusal must name the record `restrict carries, got: {result}"
        );
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "a bare `restrict must never register a child"
        );
    }

    /// Both new spellings pass the grant door — the open `grant` row carries a
    /// payload-bearing tag too — so what comes back is the *next* door's
    /// refusal, naming `provider`, and still no child.
    #[test]
    fn inherit_and_restrict_pass_the_grant_door() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        for grant in ["`inherit", "`restrict [net: false]"] {
            let (result, _) = session.ral(&format!(
                    r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: {grant}, search: true, provider: `guess, model: `inherit]"
                ),
                5,
                &emit,
            );
            assert!(
                result.contains("`provider") && !result.contains("`grant"),
                "{grant} must pass the grant door and be refused at `provider`, got: {result}"
            );
            assert!(
                crate::fleet::roster::summary(&session.agent).live == 0,
                "a later door's refusal must never leave a child registered"
            );
        }
    }

    #[test]
    fn unknown_type_tag_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `bogus, grant: `confined, search: true, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(result.contains("amnemon"), "got: {result}");
        assert!(result.contains("mnemon"), "got: {result}");
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "an unknown type tag must never register a child"
        );
    }

    /// The `provider`/`model` rows are open too, so an unrecognised arm must
    /// reach the door and be told the two that exist.
    #[test]
    fn unknown_selection_tag_errors_naming_both_arms() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `confined, search: true, provider: `guess, model: `inherit]",
            5,
            &emit,
        );
        for arm in ["inherit", "named"] {
            assert!(result.contains(arm), "must name `{arm}, got: {result}");
        }
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "an unknown selection tag must never register a child"
        );
    }

    /// An empty name is a mistake, not a way of spelling `` `inherit ``.
    #[test]
    fn an_empty_named_selection_is_refused() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `confined, search: true, provider: `inherit, model: `named '']",
            5,
            &emit,
        );
        assert!(
            result.contains("non-empty"),
            "the refusal must say the name may not be empty, got: {result}"
        );
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "an empty selection name must never register a child"
        );
    }

    #[test]
    fn invalid_name_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral(r#"exarch-agents `start [prompt: #'hi'#, name: "has space", type: `amnemon, grant: `confined, search: true, provider: `inherit, model: `inherit]"#,
            5,
            &emit,
        );
        assert!(result.contains("name"), "got: {result}");
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "an invalid name must never register a child"
        );
    }

    /// `scheme_agents`'s outer tag row (`ρ3`) is open, so an unrecognised tag
    /// must reach `builtin_agents`'s door rather than die as a row mismatch.
    #[test]
    fn unknown_outer_tag_reaches_the_door_naming_every_legal_label() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral("exarch-agents `stop 'x'", 5, &emit);
        for tag in ["list", "start", "message", "cancel"] {
            assert!(result.contains(tag), "must name `{tag}, got: {result}");
        }
    }

    /// Static, not a door error: `scheme_agents`'s closed `` `start `` record
    /// row reports which label is absent.
    #[test]
    fn missing_agent_field_errors_statically_naming_the_field() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, search: true, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.contains("field named 'grant'"),
            "the diagnostic must name the missing field, got: {result}"
        );
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "a missing spec field must never register a child"
        );
    }

    /// A misspelled field (`grnat` for `grant`) is not the same fault as an
    /// absent one: the closed `` `start `` row rejects it statically too, and
    /// must still name a field rather than shrug at the whole record.
    #[test]
    fn misspelled_start_field_errors_statically_naming_the_field() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grnat: `confined, search: true, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.contains("no field named 'grnat'"),
            "the diagnostic must name the offending field, got: {result}"
        );
        assert!(
            crate::fleet::roster::summary(&session.agent).live == 0,
            "a misspelled spec field must never register a child"
        );
    }

    /// Drives `Avatar::ral` rather than `Avatar::deliberate`'s provider loop:
    /// the spawn seeds the child's handle from the parent's *own*
    /// `Arc<Provider>`, so one script consumed by both a driven parent
    /// exchange and its child races over which gets which stage.
    #[test]
    fn agent_full_stack_round_trip_answers_the_summary_and_parks_a_reply() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "reply-1",
                    r"exarch-agents `reply 'say hi'",
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let (result, _) = session.ral(r"exarch-agents `start [prompt: #'say hi'#, name: 'helper', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.contains("live: 1"),
            "the summary answered afterwards must count the child, got: {result}"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Agent(r)) => {
                    let notice = r.outcome.marked_item(&r.name, r.elapsed);
                    assert!(
                        notice.contains("exarch-agents `read 'helper'"),
                        "the reply notice must name the fetch command, got: {notice}"
                    );
                    break;
                }
                Some(_other) => panic!("expected an Agent result item"),
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "child did not settle within the timeout"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }

        let (read, _) = session.ral(r"exarch-agents `read 'helper'", 5, &emit);
        assert!(
            read.contains("say hi"),
            "exarch-agents `read` must answer the child's deposited reply, got: {read}"
        );
        let (roster, _) = session.ral(r"exarch-agents `list", 5, &emit);
        assert!(
            roster.contains("replied"),
            "the replied child must stay on the roster as `replied, got: {roster}"
        );
    }

    /// A cancel is a request, not a transaction: it only stamps the cancel
    /// layers, and the cancelled agent's own loop is what retires it — so the
    /// row must still be listed the instant this answers.
    ///
    /// Pinned with a bare agent rather than a real spawned child: a scripted
    /// child runs to completion and settles on the same synchronous thread
    /// that starts it, so a second `Avatar::ral` racing a real
    /// `` `start ``/`` `cancel `` pair would be racing CPU-bound work with no
    /// reliable window in between.
    #[test]
    fn agents_cancel_answer_still_counts_the_cancelled_agent() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let mut doomed = crate::agent::testkit::TestAgentSpec::new("doomed");
        doomed.parent = Some(session.agent.clone());
        let _doomed = crate::agent::testkit::test_agent(&session.fleet, doomed)
            .expect("a fresh child of a live parent");

        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral("exarch-agents `cancel 'doomed'", 5, &emit);
        assert!(
            result.contains("EXIT: 0"),
            "a valid exarch-agents `cancel call must succeed, got: {result}"
        );
        assert!(
            result.contains("live: 1"),
            "a cancel is a request, not a transaction — the target is still \
             counted by the summary answered afterwards, got: {result}"
        );
    }

    // ── schedule family door tests ───────────────────────────────────────
    //
    // Tag payloads are greedy, but `at_tag_payload_end` in
    // `core/src/syntax/parser.rs` stops one at a comma — so inside a record
    // literal a nullary tag cannot swallow its neighbour. That is why
    // `` exarch-schedules `add `` takes one spec record, not three positional
    // arguments.

    #[test]
    fn bad_cron_expr_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral(
            "exarch-schedules `add [trigger: `cron '* * * *', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(
            result.contains("five fields"),
            "must carry the parser's own message, got: {result}"
        );
        assert!(
            session.agent.schedules.list().is_empty(),
            "a bad cron expression must never register a schedule"
        );
    }

    #[test]
    fn bad_duration_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral(
            "exarch-schedules `add [trigger: `after 'nope', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(
            result.contains("duration"),
            "must carry the parser's own message, got: {result}"
        );
        assert!(
            session.agent.schedules.list().is_empty(),
            "a bad duration must never register a schedule"
        );
    }

    #[test]
    fn trigger_neither_cron_nor_after_errors_before_any_enquiry_crosses() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral(
            "exarch-schedules `add [trigger: `bogus 'x', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(result.contains("cron"), "got: {result}");
        assert!(result.contains("after"), "got: {result}");
        assert!(
            session.agent.schedules.list().is_empty(),
            "an unrecognised trigger tag must never register a schedule"
        );
    }

    /// Static, not a door error: `scheme_schedules`'s closed `` `add ``
    /// record row reports which label is absent.
    #[test]
    fn missing_spec_field_errors_statically_naming_the_field() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral(
            "exarch-schedules `add [trigger: `after '1s', label: 'nightly']",
            5,
            &emit,
        );
        assert!(
            result.contains("missing a field named 'prompt'"),
            "the diagnostic must name the missing field, got: {result}"
        );
        assert!(
            session.agent.schedules.list().is_empty(),
            "a missing spec field must never register a schedule"
        );
    }

    /// The same closed row in the other direction: it admits exactly
    /// trigger/label/prompt.
    #[test]
    fn unknown_extra_spec_field_errors_statically_naming_the_field() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral("exarch-schedules `add [trigger: `after '1s', label: 'nightly', prompt: #'wake'#, extra: 1]",
            5,
            &emit,
        );
        assert!(
            result.contains("no field named 'extra'"),
            "the diagnostic must name the surplus field, got: {result}"
        );
        assert!(
            session.agent.schedules.list().is_empty(),
            "an unknown extra spec field must never register a schedule"
        );
    }

    /// A trunk holding the self-wakeup grant, which `for_test` withholds.
    fn granted_trunk() -> crate::agent::Avatar {
        crate::agent::Avatar::for_test_with(crate::agent::TestTrunk {
            allow_schedule: true,
            ..crate::agent::TestTrunk::new("system")
        })
        .expect("a granted test trunk")
    }

    #[test]
    fn schedule_add_answer_carries_the_new_row() {
        let mut session = granted_trunk();

        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let (result, _) = session.ral(
            "exarch-schedules `add [trigger: `after '10m', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(
            result.contains("nightly"),
            "the table answered afterwards must carry the row just armed, got: {result}"
        );
    }

    /// The wait is generous because the fire really is a wall-clock second
    /// away: `parse_duration`'s smallest unit is whole seconds.
    #[test]
    fn schedule_full_stack_round_trip_answers_the_table_and_fires_into_inbox() {
        let mut session = granted_trunk();

        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let (result, _) = session.ral(
            "exarch-schedules `add [trigger: `after '1s', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(
            result.contains("EXIT: 0"),
            "a valid exarch-schedules `add call must succeed, got: {result}"
        );
        assert!(
            result.contains("next-s"),
            "the table answered afterwards must carry the new row, got: {result}"
        );
        let live = session.agent.schedules.list();
        assert_eq!(live.len(), 1, "the schedule must be registered");
        assert_eq!(live[0].label, "nightly", "must take the given label");
        assert!(
            result.contains("nightly"),
            "the table must carry the given label, got: {result}"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Wakeup(text)) => {
                    assert!(
                        text.contains("wake"),
                        "the wakeup must carry the prompt, got: {text}"
                    );
                    break;
                }
                Some(_other) => panic!("expected a Wakeup item"),
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the schedule did not fire within the timeout"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    }

    /// `` `removed ``/`` `no-such-label `` are retired: `` exarch-schedules `remove ``
    /// now answers the table afterwards either way, so the row's absence is
    /// the only evidence — a miss on an already-gone label answers the same
    /// way as a hit, and that is not itself proof of a mistake.
    #[test]
    fn schedule_remove_full_stack_disarms_by_label() {
        let mut session = granted_trunk();

        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let (result, _) = session.ral(
            "exarch-schedules `add [trigger: `after '10m', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        assert!(
            result.contains("EXIT: 0"),
            "a valid exarch-schedules `add call must succeed, got: {result}"
        );
        assert_eq!(
            session.agent.schedules.list().len(),
            1,
            "the schedule must be registered"
        );

        let (result, _) = session.ral("exarch-schedules `remove 'nightly'", 5, &emit);
        assert!(
            result.contains("EXIT: 0"),
            "a valid exarch-schedules `remove call must succeed, got: {result}"
        );
        assert!(
            !result.contains("nightly"),
            "the removed row must be gone from the table answered afterwards, got: {result}"
        );
        assert!(
            session.agent.schedules.list().is_empty(),
            "exarch-schedules `remove by label must remove the schedule"
        );

        let (miss, _) = session.ral("exarch-schedules `remove 'nightly'", 5, &emit);
        assert!(
            miss.contains("EXIT: 0"),
            "removing an already-absent label answers the same empty table, not an error, got: {miss}"
        );
    }

    /// A single armed schedule cannot tell "the removed row is gone" apart
    /// from "the table is empty" — two distinguishable labels can.
    #[test]
    fn schedule_remove_answer_omits_the_removed_row_but_keeps_the_other() {
        let mut session = granted_trunk();

        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.ral(
            "exarch-schedules `add [trigger: `after '10m', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        );
        session.ral(
            "exarch-schedules `add [trigger: `after '10m', label: 'daily', prompt: #'wake'#]",
            5,
            &emit,
        );

        let (result, _) = session.ral("exarch-schedules `remove 'nightly'", 5, &emit);
        assert!(
            !result.contains("nightly"),
            "the removed row must be gone from the table answered afterwards, got: {result}"
        );
        assert!(
            result.contains("daily"),
            "the untouched row must still be in the table answered afterwards, got: {result}"
        );
    }

    // ── `reply` ───────────────────────────────────────────────────────────

    /// The record must reach the parent's inbox structured, not flattened
    /// to a string. The child's script does the replying, for the
    /// undriven-parent reason
    /// `agent_full_stack_round_trip_answers_the_roster_and_settles_into_inbox`
    /// gives.
    #[test]
    fn reply_full_stack_round_trip_delivers_structured_record_to_parent_inbox() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"let found = ["a.rs", "b.rs"]; exarch-agents `reply [files: $found]"#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let (result, _) = session.ral(r"exarch-agents `start [prompt: #'find files'#, name: 'finder', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.contains("live: 1"),
            "the summary must be the run's value and must count the child, got: {result}"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Agent(_)) => break,
                Some(_other) => panic!("expected an Agent result item"),
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "child did not settle within the timeout"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }

        let (read, _) = session.ral(r"exarch-agents `read 'finder'", 5, &emit);
        assert!(
            read.contains("files:") && read.contains("a.rs") && read.contains("b.rs"),
            "the structured record must reach the parent through `exarch-agents `read`, got: {read}"
        );
    }

    /// The refusal is an ordinary call error, not a termination: a later,
    /// well-formed `` exarch-agents `reply `` still succeeds.
    #[test]
    fn reply_refuses_a_non_first_order_value_and_does_not_terminate() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let (result, _) = session.ral(r"exarch-agents `reply { echo hi }", 5, &emit);
        assert!(
            result.contains("first-order"),
            "must name the first-order rule, got: {result}"
        );

        let (ok, _) = session.ral(r"exarch-agents `reply 42", 5, &emit);
        assert!(
            ok.contains("EXIT: 0"),
            "the session must still be usable after a refused reply, got: {ok}"
        );
    }

    #[test]
    fn double_reply_in_one_exchange_is_last_wins() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"exarch-agents `reply "first"; exarch-agents `reply "second""#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let provider_handle = session.current_provider();
        let outcome = session.deliberate(
            &provider_handle,
            Some("go".into()),
            None,
            &crate::agent::cancel::Token::new(),
            &emit,
        );
        match outcome {
            Ok(crate::agent::deliberate::Outcome::Replied(v)) => {
                assert_eq!(
                    v,
                    ral_core::serial::FOValue::String {
                        value: "second".into()
                    },
                    "the last reply in the exchange must win"
                );
            }
            other => panic!("expected Replied, got {other:?}"),
        }
    }

    // ── `exarch-pins` ────────────────────────────────────────────────────

    /// The scripted-provider round-trip pattern of
    /// `reply_full_stack_round_trip_delivers_structured_record_to_parent_inbox`,
    /// crossed with the desk's `` `exarch-pins `read `` arm: the child pins
    /// with `` `set ``, reads its own pin back in the same run, and hands the
    /// canonical card to its parent.
    #[test]
    fn pin_read_full_stack_round_trip_returns_canonical_card_to_parent() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"exarch-pins `set [key: "note", body: `card ["hi there"]]; exarch-agents `reply !{exarch-pins `read "note"}"#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let (result, _) = session.ral(r"exarch-agents `start [prompt: #'pin and read back'#, name: 'pinner', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.contains("live: 1"),
            "the summary must be the run's value and must count the child, got: {result}"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Agent(r)) => {
                    assert!(
                        matches!(r.outcome, crate::bus::AgentOutcome::Replied),
                        "the child must have replied, got: {:?}",
                        r.outcome
                    );
                    break;
                }
                Some(_other) => panic!("expected an Agent result item"),
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "child did not settle within the timeout"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }

        // The pretty-printer elides past its depth cap, so the span text
        // itself does not survive to this rendering; what proves the round
        // trip *canonical* — a lifted `` `text `` mark, not the bare-string
        // sugar it was authored with — does.
        let (read, _) = session.ral(r"exarch-agents `read 'pinner'", 5, &emit);
        assert!(
            read.contains("`card") && read.contains("`text [spans:"),
            "the canonical card must reach the parent, got: {read}"
        );
    }

    /// An absent key answers `Unit`, which crosses to the parent as `reply`'s
    /// empty rendering.
    #[test]
    fn pin_read_full_stack_absent_key_replies_unit() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"exarch-agents `reply !{exarch-pins `read "nope"}"#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let (result, _) = session.ral(r"exarch-agents `start [prompt: #'read an absent key'#, name: 'reader', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.contains("live: 1"),
            "the summary must be the run's value and must count the child, got: {result}"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Agent(r)) => {
                    assert!(
                        matches!(r.outcome, crate::bus::AgentOutcome::Replied),
                        "the child must have replied, got: {:?}",
                        r.outcome
                    );
                    break;
                }
                Some(_other) => panic!("expected an Agent result item"),
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "child did not settle within the timeout"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }

        let (read, _) = session.ral(r"exarch-agents `read 'reader'", 5, &emit);
        assert!(
            read.contains("reply: ()"),
            "an absent key must reply unit, got: {read}"
        );
    }

    // ── the task kit as a pure prelude over the pin family ─────────────────

    /// Every mutating tag reads and writes the "tasks" pin through `tasks-sync`,
    /// so `` exarch-tasks `list `` and a direct
    /// `` tasks-decode !{exarch-pins `read "tasks"} `` must agree on every
    /// field, tags and notes included.
    #[test]
    fn kit_round_trip_holds_every_field_including_tags_and_notes() {
        // Seven evals deep where this file's other tests run one or two, so a
        // debug build sharing the box with the rest of the suite needs more
        // room than the usual 5s: a call that times out here reads as a lost
        // field, not as a slow machine.
        const BUDGET: u64 = 60;

        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.ral(r#"exarch-tasks `add "fix the parser""#, BUDGET, &emit);
        session.ral(r#"exarch-tasks `add "write docs""#, BUDGET, &emit);
        session.ral(
            "exarch-tasks `status [id: 1, status: `doing]",
            BUDGET,
            &emit,
        );
        session.ral(r#"exarch-tasks `tag [id: 1, tag: "urgent"]"#, BUDGET, &emit);
        session.ral(
            r#"exarch-tasks `note [id: 1, note: "blocked on review"]"#,
            BUDGET,
            &emit,
        );

        let (listed, _) = session.ral("exarch-tasks `list", BUDGET, &emit);
        for field in ["fix the parser", "`doing", "urgent", "blocked on review"] {
            assert!(
                listed.contains(field),
                "exarch-tasks `list must show the tagged, noted task's {field}, got: {listed}"
            );
        }
        assert!(
            listed.contains("write docs"),
            "exarch-tasks `list must show the untouched second task, got: {listed}"
        );

        let (read, _) = session.ral(
            r#"let [decoded-task, _] = !{tasks-decode !{exarch-pins `read "tasks"}}
               echo $decoded-task[desc]
               echo $decoded-task[status]
               echo !{intercalate "," $decoded-task[tags]}
               echo $decoded-task[notes]"#,
            BUDGET,
            &emit,
        );
        assert!(
            read.contains("fix the parser"),
            "the decoded desc must survive, got: {read}"
        );
        assert!(
            read.contains("doing"),
            "the decoded status must survive, got: {read}"
        );
        assert!(
            read.contains("urgent"),
            "the decoded tags must survive, got: {read}"
        );
        assert!(
            read.contains("blocked on review"),
            "the decoded notes must survive, got: {read}"
        );
    }

    /// `` exarch-tasks `add `` inside a function body pins to the register, which SPEC
    /// §10's block-discard rule never touches — a later, separate top-level
    /// run still sees it.
    #[test]
    fn add_task_inside_a_function_body_survives_the_block_and_the_call() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.ral(
            r#"let f = { exarch-tasks `add "inside a block" }; !{f}"#,
            5,
            &emit,
        );

        let (listed, _) = session.ral("exarch-tasks `list", 5, &emit);
        assert!(
            listed.contains("inside a block"),
            "a task added inside a function body must survive to the next top-level run, got: {listed}"
        );
    }

    /// A sub-agent's register is its own: a child's `` exarch-tasks `add `` must never
    /// reach the parent's "tasks" pin.
    #[test]
    fn sub_agent_pinning_tasks_leaves_the_parents_register_untouched() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.ral(r#"exarch-tasks `add "parent task""#, 5, &emit);

        let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new().then(
                crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                    "c1",
                    r#"exarch-tasks `add "child task"; exarch-agents `reply "done""#,
                )]),
            ),
        ));
        session.provider_handle().swap(provider);

        let (result, _) = session.ral(r"exarch-agents `start [prompt: #'add a task'#, name: 'tasker', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
            5,
            &emit,
        );
        assert!(
            result.contains("live: 1"),
            "the summary must be the run's value and must count the child, got: {result}"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match session.next_item_for_test() {
                Some(crate::bus::Item::Agent(_)) => break,
                Some(_other) => panic!("expected an Agent result item"),
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "child did not settle within the timeout"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }

        let (listed, _) = session.ral("exarch-tasks `list", 5, &emit);
        assert!(
            listed.contains("parent task"),
            "the parent's own task must survive, got: {listed}"
        );
        assert!(
            !listed.contains("child task"),
            "the child's pin must never reach the parent's register, got: {listed}"
        );
    }

    /// `tasks-sync` clears the slot once no work remains: transitioning the
    /// last open task to `` `done `` empties the pin, and a later `` exarch-tasks `add ``
    /// finds no register and restarts id allocation at 1.
    #[test]
    fn transitioning_the_last_open_task_to_done_clears_the_pin_and_restarts_ids() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.ral(r#"exarch-tasks `add "only task""#, 5, &emit);
        session.ral("exarch-tasks `status [id: 1, status: `done]", 5, &emit);

        let (read, _) = session.ral(r#"exarch-pins `read "tasks""#, 5, &emit);
        assert!(
            !read.contains("VALUE:"),
            "an all-done list must clear the pin to unit, got: {read}"
        );

        session.ral(r#"exarch-tasks `add "fresh""#, 5, &emit);
        let (listed, _) = session.ral("exarch-tasks `list", 5, &emit);
        for field in ["id: 1", "fresh", "`open"] {
            assert!(
                listed.contains(field),
                "id allocation must restart at 1 once the register is empty, missing {field} in: {listed}"
            );
        }
    }

    /// A card under "tasks" that `tasks-decode` does not recognise — the
    /// model scribbled on the shared key — fails the next kit call with the
    /// didactic message naming the expected shape, rather than corrupting or
    /// silently discarding it.
    #[test]
    fn a_foreign_card_under_tasks_fails_the_kit_didactically() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        session.ral(r#"exarch-pins `set [key: "tasks", body: `card [`text [spans: [[text: "not task shaped"]]]]]"#,
            5,
            &emit,
        );

        let (result, _) = session.ral(r#"exarch-tasks `add "x""#, 5, &emit);
        assert!(
            result.contains("tasks: the card under the 'tasks' pin is not task-shaped"),
            "the didactic fail must name the expected shape, got: {result}"
        );
    }

    // ── context family door tests ────────────────────────────────────────

    /// A trunk holding one answered prompt, which every context test needs
    /// before it has anything addressable to name.
    fn trunk_with_an_answered_prompt() -> crate::agent::Avatar {
        let session = crate::agent::Avatar::for_test("system").unwrap();
        crate::agent::testkit::close_exchange(&session, "first prompt", "first answer");
        session
    }

    /// `scheme_context`'s outer tag row is open, so `` exarch-context `rewind `` — the
    /// tag a model most plausibly invents — reaches the door naming the two
    /// legal ones rather than dying as a row-unification mismatch.
    #[test]
    fn unknown_context_tag_reaches_the_door_naming_every_legal_tag() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral("exarch-context `rewind [3]", 5, &emit);
        for tag in ["survey", "evict"] {
            assert!(result.contains(tag), "must name `{tag}, got: {result}");
        }
    }

    /// `` `evict ``'s record row is open only on the tail, so `turns` itself
    /// is still static: a misspelling reaches the type error, not the door.
    #[test]
    fn misspelled_evict_field_errors_statically_naming_the_field() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral("exarch-context `evict [turn: [1]]", 5, &emit);
        assert!(
            !result.contains("EXIT: 0") && result.contains("turns"),
            "the diagnostic must name the field the row demands, got: {result}"
        );
    }

    /// The whole family through the real shell: an edit answers the survey
    /// the transition leaves behind, so the count of what left is there to
    /// read without a second call. The address names turns — 1 and 2 are the
    /// first prompt and its reply — and the optional `note` rides the
    /// open record row, so both shapes type-check.
    #[test]
    fn an_eviction_answers_the_survey_it_leaves_behind() {
        let mut session = trunk_with_an_answered_prompt();
        crate::agent::testkit::close_exchange(&session, "second prompt", "second answer");
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let (result, _) = session.ral(
            "exarch-context `evict [turns: [1, 2], note: 'the old work is done']",
            5,
            &emit,
        );
        assert!(
            result.contains("EXIT: 0"),
            "a valid eviction must succeed, got: {result}"
        );
        assert!(
            result.contains("total-bytes"),
            "the answer must be the survey afterwards, got: {result}"
        );
        assert!(
            !result.contains("bytes-delta"),
            "the edit answers the state, never a receipt for the transition, got: {result}"
        );
    }

    /// The same tag with no `note`: the open record row admits it, so the
    /// harness never asks the model for an empty string.
    #[test]
    fn an_eviction_without_a_note_type_checks() {
        let mut session = trunk_with_an_answered_prompt();
        crate::agent::testkit::close_exchange(&session, "second prompt", "second answer");
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let (result, _) = session.ral("exarch-context `evict [turns: !{range 1 3}]", 5, &emit);
        assert!(
            result.contains("EXIT: 0"),
            "an eviction with no note must succeed, got: {result}"
        );
    }

    /// `` `read ``'s answer is a list, so a slice is `$read[0]`, naming its
    /// own `turn` and the `role` it bears, and its messages are ral
    /// records with variant parts rather than a rendered string.
    #[test]
    fn transcript_answers_turn_records_with_variant_parts() {
        let mut session = trunk_with_an_answered_prompt();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let (result, _) = session.ral(
            r#"let material = exarch-transcript `read [turns: [1, 2]]
               echo !{length $material}
               echo $material[0][turn] $material[0][role]
               let msgs = $material[0][messages]
               let say-role = { |r| case $r [
                 `system: { |_| echo "role=system" },
                 `user: { |_| echo "role=user" },
                 `assistant: { |_| echo "role=assistant" },
                 `tool: { |_| echo "role=tool" },
               ] }
               let say-part = { |p| case $p [
                 `text: { |[content: c]| echo "text=$c" },
                 `program: { |_| echo "part=program" },
                 `result: { |_| echo "part=result" },
                 `reasoning: { |_| echo "part=reasoning" },
                 `binary: { |_| echo "part=binary" },
                 `custom: { |_| echo "part=custom" },
               ] }
               say-role $msgs[0][role]
               say-part $msgs[0][parts][0]
               let reply = $material[1][messages]
               say-role $reply[0][role]
               say-part $reply[0][parts][0]"#,
            5,
            &emit,
        );
        assert!(
            result.contains("\n2\n"),
            "two named turns, one record each, got: {result}"
        );
        assert!(
            result.contains("1 user"),
            "the first record names its own turn and its role, got: {result}"
        );
        assert!(
            result.contains("role=user") && result.contains("text=first prompt"),
            "the user turn must be a `text part of a `user message, got: {result}"
        );
        assert!(
            result.contains("role=assistant") && result.contains("text=first answer"),
            "the assistant turn must be a `text part of an `assistant message, got: {result}"
        );
    }

    /// The door speaks turn ids, so an address built with `range` and one
    /// written out read the same turns, each record naming the role it bears
    /// — a set that spans a prompt boundary answers one record per turn.
    #[test]
    fn transcript_read_addresses_turns_wherever_they_lie() {
        let mut session = trunk_with_an_answered_prompt();
        crate::agent::testkit::close_exchange(&session, "second prompt", "second answer");
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let (result, _) = session.ral(
            r"let spanning = exarch-transcript `read [turns: [2, 3]]
               echo !{length $spanning}
               echo $spanning[0][turn] $spanning[0][role] !{length $spanning[0][messages]}
               echo $spanning[1][turn] $spanning[1][role]
               let built = exarch-transcript `read [turns: !{range 3 5}]
               echo !{length $built} $built[1][turn]",
            5,
            &emit,
        );
        assert!(
            result.contains("\n2\n"),
            "two named turns answer one record each, got: {result}"
        );
        assert!(
            result.contains("2 assistant 1"),
            "turn 2 is an assistant turn holding one message, got: {result}"
        );
        assert!(
            result.contains("3 user"),
            "turn 3 is the prompt after it, got: {result}"
        );
        assert!(
            result.contains("2 4"),
            "`range 3 5` is turns 3 and 4, the second of them turn 4, got: {result}"
        );
    }

    /// `scheme_transcript`'s outer tag row is open, so `` exarch-transcript `search ``
    /// — the tag a model most plausibly invents — reaches the door naming the
    /// three legal ones.
    #[test]
    fn unknown_transcript_tag_reaches_the_door_naming_every_legal_tag() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (result, _) = session.ral("exarch-transcript `search 'x'", 5, &emit);
        for tag in ["index", "read", "grep"] {
            assert!(result.contains(tag), "must name `{tag}, got: {result}");
        }
    }

    /// The answer type is free, so each tag's own shape has to type-check
    /// against the use the program makes of it: an index row carries `id` and
    /// `held`, and a grep answer projects `hits` and `total` as a record, each
    /// hit naming the turn it lies in. The optional `turns` rides `` `grep ``'s
    /// open record row.
    #[test]
    fn index_and_grep_answer_their_own_shapes() {
        let mut session = trunk_with_an_answered_prompt();
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);

        let (result, _) = session.ral(
            // Bindings are named, not lettered: ral keeps value and command
            // names disjoint, so a one-letter binding fails on any host with
            // that letter on PATH (plan9port ships a `g`).
            r"let listed = exarch-transcript `index
               echo $listed[0][id] $listed[0][role] $listed[0][held]
               let matched = exarch-transcript `grep [pattern: 'first (prompt|answer)']
               echo !{length $matched[hits]} $matched[total]
               echo $matched[hits][0][turn] $matched[hits][1][turn]
               let narrowed = exarch-transcript `grep [pattern: 'first', turns: [2]]
               echo $narrowed[total]
               let missed = exarch-transcript `grep [pattern: 'nothing here', turns: [1, 2]]
               echo $missed[total]",
            5,
            &emit,
        );
        assert!(
            result.contains("1 user resident"),
            "the first turn is listed and still in the context, got: {result}"
        );
        assert!(
            result.contains("2 2"),
            "both of its turns match, and `total` counts them all, got: {result}"
        );
        assert!(
            result.contains("1 2"),
            "the hits name the turns they lie in, got: {result}"
        );
        assert!(
            result.contains("\n1\n"),
            "an address narrows the search to the assistant turn alone, got: {result}"
        );
        assert!(
            result.contains("\n0\n"),
            "a pattern that matches nothing answers no hits, got: {result}"
        );
    }
}
