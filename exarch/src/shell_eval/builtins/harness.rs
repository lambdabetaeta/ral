//! The harness builtins — `exarch-agents`, `exarch-schedules`, `exarch-pins`,
//! `exarch-context`, `exarch-transcript` — with the type schemes that gate
//! them. A returning agent's reply is a tag of `exarch-agents`
//! (`` `reply ``), not a builtin of its own — the fleet is one family.
//!
//! Each door reads the model's argument as a [`Request`] before it enquires,
//! so a malformed call never reaches the host, and is refused in the words
//! the desk would use. `exarch-agents`'s `` `start `` tag, and the host-only
//! `_exarch-branch`, fork this shell and tell the host how to reach the fork,
//! which is what the run's [`Fork`] door says: an
//! in-process host adopts a fork parked in the run's nursery, since the
//! reentrancy law bars a desk handler from holding `&mut Shell` to fork one
//! itself; a host across a wire is handed a guest port to dial, and dials it
//! while it answers. [`crate::agent::desk::ExarchDesk`] answers every enquiry
//! on the other side.
//!
//! All but `exarch-transcript` name a state, and answer it afterwards.
//! `exarch-transcript` names the record instead, which no tag of it writes,
//! so its tags answer what they were asked for rather than a state.

use crate::enquiry::{
    Agents, Context, Evict, Family, ForkClaim, Pins, Reading, Request, Schedules, Start, Survey,
    Transcript, Turns, Word, family,
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
    site.admit(&answer)
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
            crate::shell_eval::arm_session_ledgers(&mut fork);
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
    let listener = ral_core::seed::hatch::listen_for_hatch(socket, token, shell, grant)
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
        Err(ral_core::seed::hatch::Unhatched::Failed(reason)) => {
            Err(sig(format!("{AGENTS}: {reason}")))
        }
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
        "{AGENTS}: this engine has no hatch support outside a Linux guest; a wire trunk's \
         helper spawn only ever reaches one"
    )))
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

/// `exarch-agents :: ∀β:data α ρ1^d ρ2^d ρ3^d ρ4^d ρ5^d. <list | start [prompt: Str, name: Str, type: Variant ρ1, grant: Variant ρ2, search: Bool, provider: Variant ρ3, model: Variant ρ4] | message [to: Str, text: Str] | cancel Str | reply β | read Str | ρ5> → F α`
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
        "exarch-agents <tag>  — agent control: `list extant agents, `start a new one, `message them, `cancel them, `reply to your parent, `read the reply of a descendant.\n\nexarch-agents `list: every extant agent (including yourself), along with their `spawner` (or `root). `state` is `busy while working, `waiting-on-agents while held only by a descendant, `replied, or `waiting if it is parked but has not replied. `idle-s` is seconds since it parked.\n\nexarch-agents `start [prompt: <Str>, name: <Str>, type: `amnemon|`mnemon, grant: <permission>, search: <Bool>, provider: `inherit|`named <Str>, model: `inherit|`named <Str>]  — launch an agent, asynchronously; a reply will arrive later as a notice. An agent with an `amnemon` `type` starts with an empty context, while `mnemon` inherits yours (also reusing cache). Every new agent has the same bindings, cwd, and env as you, except live job handles. `prompt` is the agent's prompt, and can be computed as part of a ral script. `name` is the child's identity (non-empty, at most 24 characters, unique, ASCII only). `grant` controls permissions: `inherit (same as your authority), `confined (offline, no home reads), `read-only (may write to scratch), `edit-only (edits the cwd, no build tooling), `reasonable (everyday tooling), or `restrict <record>, which takes a capability record of the same shape as `grant [...]` (exec, fs, net, detach, editor, shell); note that this may only restrict authority, not expand it. `search` states whether the child may use web search. `provider` and `model` control the inference provider and model: `inherit shares your own, and `named specifies a different one. A model the provider does not list is refused.\n\nexarch-agents `message [to: <Str>, text: <Str>]  — send `text` as a marked item to the agent named `to`.\n\nexarch-agents `cancel <name>; stop a descendant agent as soon as possible.\n\nexarch-agents `reply <value>  — hand `value` back to your parent agent. `value` must be first-order data (no blocks, handles, or environments).\n\nexarch-agents `read <name>  — fetch the value the descendant named `name` last handed to `reply, as [name: Str, reply: <value>].",
        builtin_agents,
    ),
    BuiltinEntry::new(
        Cow::Borrowed("exarch-schedules"),
        scheme_schedules,
        "exarch-schedules <tag>  — your self-wakeups: `list what is armed, `add one, `remove one. Every tag answers with the table afterwards, [[label: Str, trigger: Str, next-s: Int, fires: Int]], so what you read back is always what is armed now rather than a receipt for what you just did. Requires the self-wakeup grant (--allow-schedule): an agent that can wake itself indefinitely holds real authority, so without the grant every tag is refused.\n\nexarch-schedules `list  — your live wakeups, oldest first: label as you named it, trigger as its source text (a cron expression, or `after 30m`), next-s the seconds until the next fire, recomputed as you ask, and fires how many times it has fired so far. Only live schedules appear: a spent one-shot has already removed itself, so a label you armed with `after and then see no more of has fired, not vanished. This is how you recover labels after an eviction.\n\nexarch-schedules `add [trigger: `cron <Str>|`after <Str>, label: <Str>, prompt: <Str>]  — arm a self-wakeup: at the chosen time a marked item carrying `prompt` is delivered to your inbox and re-engages you with no human present. It drains at your next exchange boundary (as soon as the tool batch in flight settles, not only at the end of the exchange) and arrives as marked chrome, `[scheduled '<label>' · <trigger>] <prompt>`, never read as a command even when the prompt opens with `/`. `trigger` is exactly one of two variants; any other shape is refused, naming both. `cron '<expr>'` is recurring: five whitespace-separated fields, minute hour day-of-month month day-of-week, read in the host's local timezone, e.g. `cron '0 9 * * 1-5'` for weekdays at 09:00. Each field is a comma list of `*`, a number, a range `a-b`, or a step over either (`*/15`, `a-b/2`, `N/step` meaning N up to the field's maximum); month and day-of-week also accept three-letter names (jan…dec, sun…sat), and day-of-week accepts 7 as a second spelling of Sunday. When both day fields are restricted, either one matching fires it (Vixie-cron's OR rule); when only one is, that one decides. Every fire recomputes the next occurrence in the host timezone, so DST shifts, clock steps, and suspends are absorbed rather than accumulated. `after '<n><unit>'` is a one-shot relative delay from the moment of arming, unit one of s/m/h/d and the count greater than zero, e.g. `after '30m'`, `after '2h'`. A trigger with no next occurrence at all (a parseable but impossible date such as `cron '0 0 30 2 *'`) is refused here rather than arming silently. `label` names the wakeup and is its identity: it must not be borne by another live schedule, and you must always supply one. `prompt` is the natural-language instruction you act on when woken, not code. Read the new row's next-s out of the answer to catch a cron expression that parsed but does not mean what you meant. Once armed: an `after removes itself when it fires; a cron re-arms itself, and drops itself only when nothing further lies inside its search horizon. A fire whose previous wakeup is still sitting undrained in your inbox is skipped, not queued behind it, and does not count as a fire. While any schedule is live this session parks for the next wakeup at quiescence instead of ending, so a recurring schedule you never remove keeps this agent alive indefinitely; that is what the grant buys. `/clear` drops every live schedule.\n\nexarch-schedules `remove <label>  — disarm the wakeup bearing `label`; its next occurrence goes with it and nothing further is delivered. The entry is gone in the answer, so the row's absence is the confirmation. A label that was never there answers the same way, and that is no evidence of a mistake: a one-shot may have fired and removed itself since you read it.\n\nEach tag is one exchange with the host, and the table it answers is the schedule registry as it stands once the transition has landed. A raise still does not prove nothing happened: the transition may have landed and its answer failed to reach you. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_family::<Schedules>),
    ),
    BuiltinEntry::boundary(
        Cow::Borrowed("exarch-pins"),
        scheme_pins,
        "exarch-pins <tag>  — your register of pinned state: a small set of named slots that outlive any one exchange. `set` writes a slot, `clear` empties one, `read` fetches a slot back, `list` names every occupied one. Reads and writes your own register only.\n\nexarch-pins `set [key: <Str>, body: <card>]  — overwrite the register slot named `key` with `body`, a `card [...]` value (or one of its marks bare, e.g. `text [...]`).\n\nexarch-pins `clear <key>  — empty the register slot named `key`. Clearing an already-empty slot is not an error.\n\nexarch-pins `read <key>  — `some` the card currently pinned under `key`, as a `card value you can destructure, or `none if the slot is empty.\n\nexarch-pins `list  — the keys currently occupied on your register, as [String]. Read one back with `read`.\n\nEach tag is one exchange with the host. Answered only on the run that calls it: inside spawn { … } this errors.",
        boundary_family::<Pins>,
    ),
    BuiltinEntry::new(
        Cow::Borrowed("exarch-context"),
        scheme_context,
        "exarch-context <tag>  — the context: the messages the provider is sent on your next request, as a list of turns. A turn is either a user turn (a prompt, or an import's opening) or an assistant turn (one assistant message, the tool results it called for, and any steering delivered before the next request). Turn ids are minted in one increasing sequence per lineage and never reused; every tool result ends with `TURN: <id>`, the id of the assistant turn it closes, and `exarch-context `survey` lists the rest. Every tag answers the survey after it has acted: [rows: [[id: Int, role: Str, kind: Str, label: Str, bytes: Int]], total-bytes: Int].\n\nexarch-context `survey  — acts on nothing. `rows` is one row per turn in the context, oldest first: `id` the turn's id; `role` `user` or `assistant`; `kind` `own` (recorded by this session), `import` (a note the harness imported, e.g. on resume), or `inherited` (recorded by an ancestor before you were forked); `label` the first 50 characters of the turn's first line, or of the `description` its first tool call declared where no prose opened one; `bytes` the serialised size of the turn's messages. `total-bytes` is the serialised size of what is actually sent (the resident turns plus every marker) and is the figure to weigh against the provider's context window.\n\nexarch-context `evict [turns: [Int], note: `none|`some Str]  — removes the named turns from the context. `turns` is a list of turn ids in any order, repeats ignored; `!{range 41 44}` is [41, 42, 43]. Refused, naming the turn: an id never recorded; an id that has already left; the id of the turn being written now, i.e. the assistant turn whose result this call is part of. Kept silently: a user turn while any assistant turn answering it (the assistant turns between it and the next user turn) is in the context and not named; a set left empty by this rule is refused. Every other named turn leaves at once, wherever it lies. Where a run of consecutive turns has left, the context carries one marker in their place stating which turns left, one line per turn (id, role, label, KB; at most 40 lines per marker, older ones collapsed to a count), the note of the eviction that took them, and how to read them back. `note` is `none for no note, or `some with one line of at most 240 bytes, which appears verbatim in that marker. Evicted turns remain in the transcript and are readable with `exarch-transcript `read`. Cost: the provider's cache holds only the prefix before the earliest change, so the next request re-reads everything from the first evicted turn onward.\n\nWhen the context nears the provider's window, the harness evicts the oldest turns itself at the next turn boundary, without a note; as the context grows into the reserve before that point you are warned once, at a tool boundary, naming the turns the cut would take. Making that cut yourself is how a note gets attached.\n\nEach tag is one exchange with the host, and the survey it answers is the context as it stands once the transition has landed; an eviction lands at the desk immediately and is recorded. A raise still does not prove nothing happened: the transition may have landed and its answer failed to reach you. Answered only on the run that calls it: inside spawn { … } this errors.",
        BuiltinBody::Static(builtin_family::<Context>),
    ),
    BuiltinEntry::boundary(
        Cow::Borrowed("exarch-transcript"),
        scheme_transcript,
        "exarch-transcript <tag>  — the complete record of this session, including turns that have been evicted from the context. Turns are specified as a list (e.g. `!{range 41 44}` or [41, 42, 43]). `exarch-context `survey` and `exarch-transcript `index` show the ids.\n\nexarch-transcript `index  — [[id: Int, role: Str, kind: Str, label: Str, bytes: Int, held: Str]], every recorded turn in order. The first five fields are the survey's; `held` is `resident` (in the context) or `evicted` (left it).\n\nexarch-transcript `read [turns: [Int]]  — [[turn: Int, role: Str, messages: [Message]]], one element per named turn in id order, each turn's messages exactly as the provider was sent them. A message is [role: `system|`user|`assistant|`tool, parts: [Part]]. A Part is one of: `text [content: Str]; `program [tool: Str, source: Str, keys: [Str]]: a tool call, where for the ral tool `source` is the script and `keys` is empty, and for any other tool `source` is empty and `keys` names its arguments; `result [content: Str]: a tool result as the model saw it; `reasoning [content: Str]: reasoning in full; `binary [content-type: Str, name: Str, bytes: Int]: a binary attachment's metadata (not the content); `custom [provider: Str, model: Str]: a provider extension's identity. \n\nexarch-transcript `grep [pattern: Str, turns: `all|`only [Int]]  — [hits: [[turn: Int, role: Str, line: Int, text: Str]], total: Int]: every line of every message in the searched turns matching `pattern` (a Rust regex). `turns` is `all to search every recorded turn, or `only followed by the list of turns to search. `hits` holds at most the 100 oldest matches, each `text` clipped to 200 bytes, `line` 1-based within its message; `total` is the count of all matches. `role` is the message's role.",
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
