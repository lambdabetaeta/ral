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
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `bogus, search: true, provider: `inherit, model: `inherit]",
        5,
        &emit,
    ).text;
    for label in bare_grant_tags() {
        assert!(result.contains(label), "must name `{label}`, got: {result}");
    }
    assert!(
        crate::agent::roster::summary(&session.agent).live == 0,
        "an unknown grant label must never register a child"
    );
}

/// `` `dangerous `` has left the spawn surface: there it resolved to ⊤, a
/// layer saying nothing, which is what `` `inherit `` now says outright —
/// so the refusal must send a model there rather than leave it guessing.
#[test]
fn dangerous_is_no_longer_a_spawn_grant_and_the_refusal_points_at_inherit() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `dangerous, search: true, provider: `inherit, model: `inherit]",
        5,
        &emit,
    ).text;
    assert!(
        result.contains("inherit is how you decline to narrow"),
        "the refusal must point at `inherit, got: {result}"
    );
    assert!(
        crate::agent::roster::summary(&session.agent).live == 0,
        "`dangerous must never register a child"
    );
}

/// `` `restrict `` is the one grant tag that takes a payload, so bare it is
/// a shape error, and the refusal must say what it was missing.
#[test]
fn a_bare_restrict_errors_before_any_enquiry_crosses() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `restrict, search: true, provider: `inherit, model: `inherit]",
        5,
        &emit,
    ).text;
    assert!(
        result.contains("capability record"),
        "the refusal must name the record `restrict carries, got: {result}"
    );
    assert!(
        crate::agent::roster::summary(&session.agent).live == 0,
        "a bare `restrict must never register a child"
    );
}

/// Both new spellings pass the grant door — the open `grant` row carries a
/// payload-bearing tag too — so what comes back is the *next* door's
/// refusal, naming `provider`, and still no child.
#[test]
fn inherit_and_restrict_pass_the_grant_door() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    for grant in ["`inherit", "`restrict [net: false]"] {
        let result = session.ral(&format!(
                r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: {grant}, search: true, provider: `guess, model: `inherit]"
            ),
            5,
            &emit,
        ).text;
        assert!(
            result.contains("`provider") && !result.contains("`grant"),
            "{grant} must pass the grant door and be refused at `provider`, got: {result}"
        );
        assert!(
            crate::agent::roster::summary(&session.agent).live == 0,
            "a later door's refusal must never leave a child registered"
        );
    }
}

#[test]
fn unknown_type_tag_errors_before_any_enquiry_crosses() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `bogus, grant: `confined, search: true, provider: `inherit, model: `inherit]",
        5,
        &emit,
    ).text;
    assert!(result.contains("amnemon"), "got: {result}");
    assert!(result.contains("mnemon"), "got: {result}");
    assert!(
        crate::agent::roster::summary(&session.agent).live == 0,
        "an unknown type tag must never register a child"
    );
}

/// The `provider`/`model` rows are open too, so an unrecognised arm must
/// reach the door and be told the two that exist.
#[test]
fn unknown_selection_tag_errors_naming_both_arms() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `confined, search: true, provider: `guess, model: `inherit]",
        5,
        &emit,
    ).text;
    for arm in ["inherit", "named"] {
        assert!(result.contains(arm), "must name `{arm}, got: {result}");
    }
    assert!(
        crate::agent::roster::summary(&session.agent).live == 0,
        "an unknown selection tag must never register a child"
    );
}

/// An empty name is a mistake, not a way of spelling `` `inherit ``.
#[test]
fn an_empty_named_selection_is_refused() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `confined, search: true, provider: `inherit, model: `named '']",
        5,
        &emit,
    ).text;
    assert!(
        result.contains("non-empty"),
        "the refusal must say the name may not be empty, got: {result}"
    );
    assert!(
        crate::agent::roster::summary(&session.agent).live == 0,
        "an empty selection name must never register a child"
    );
}

#[test]
fn invalid_name_errors_before_any_enquiry_crosses() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session.ral(r#"exarch-agents `start [prompt: #'hi'#, name: "has space", type: `amnemon, grant: `confined, search: true, provider: `inherit, model: `inherit]"#,
        5,
        &emit,
    ).text;
    assert!(result.contains("name"), "got: {result}");
    assert!(
        crate::agent::roster::summary(&session.agent).live == 0,
        "an invalid name must never register a child"
    );
}

/// `scheme_agents`'s outer tag row (`ρ3`) is open, so an unrecognised tag
/// must reach `builtin_agents`'s door rather than die as a row mismatch.
#[test]
fn unknown_outer_tag_reaches_the_door_naming_every_legal_label() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session.ral("exarch-agents `stop 'x'", 5, &emit).text;
    for tag in ["list", "start", "message", "cancel"] {
        assert!(result.contains(tag), "must name `{tag}, got: {result}");
    }
}

/// Static, not a door error: `scheme_agents`'s closed `` `start `` record
/// row reports which label is absent.
#[test]
fn missing_agent_field_errors_statically_naming_the_field() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, search: true, provider: `inherit, model: `inherit]",
        5,
        &emit,
    ).text;
    assert!(
        result.contains("field named 'grant'"),
        "the diagnostic must name the missing field, got: {result}"
    );
    assert!(
        crate::agent::roster::summary(&session.agent).live == 0,
        "a missing spec field must never register a child"
    );
}

/// A misspelled field (`grnat` for `grant`) is not the same fault as an
/// absent one: the closed `` `start `` row rejects it statically too, and
/// must still name a field rather than shrug at the whole record.
#[test]
fn misspelled_start_field_errors_statically_naming_the_field() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session.ral(r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grnat: `confined, search: true, provider: `inherit, model: `inherit]",
        5,
        &emit,
    ).text;
    assert!(
        result.contains("no field named 'grnat'"),
        "the diagnostic must name the offending field, got: {result}"
    );
    assert!(
        crate::agent::roster::summary(&session.agent).live == 0,
        "a misspelled spec field must never register a child"
    );
}

/// Drives `Avatar::ral` rather than `Avatar::deliberate`'s provider loop:
/// the spawn seeds the child's handle from the parent's *own*
/// `Arc<Provider>`, so one script consumed by both a driven parent
/// exchange and its child races over which gets which stage.
#[test]
fn agent_full_stack_round_trip_answers_the_summary_and_parks_a_reply() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
        "test-model",
        crate::provider::scripted::Script::new().then(
            crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                "reply-1",
                r"exarch-agents `reply 'say hi'",
            )]),
        ),
    ));
    session.agent.provider.swap(provider);
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    let result = session.ral(r"exarch-agents `start [prompt: #'say hi'#, name: 'helper', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
        5,
        &emit,
    ).text;
    assert!(
        result.contains("live: 1"),
        "the summary answered afterwards must count the child, got: {result}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match session.next_item_for_test() {
            Some(crate::bus::Next::Item(crate::bus::Item::Agent(r))) => {
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

    let read = session.ral(r"exarch-agents `read 'helper'", 5, &emit).text;
    assert!(
        read.contains("say hi"),
        "exarch-agents `read` must answer the child's deposited reply, got: {read}"
    );
    let roster = session.ral(r"exarch-agents `list", 5, &emit).text;
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
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let mut doomed = crate::agent::testkit::TestAgentSpec::new("doomed");
    doomed.parent = Some(session.agent.clone());
    let _doomed = crate::agent::testkit::test_agent(&session.fleet, doomed)
        .expect("a fresh child of a live parent");

    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session.ral("exarch-agents `cancel 'doomed'", 5, &emit).text;
    assert!(
        result.contains("EXIT: 0"),
        "a valid exarch-agents `cancel call must succeed, got: {result}"
    );
    assert!(
        result.contains("live: 1"),
        "a cancel is a request, not a transaction: the target is still \
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
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session
        .ral(
            "exarch-schedules `add [trigger: `cron '* * * *', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        )
        .text;
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
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session
        .ral(
            "exarch-schedules `add [trigger: `after 'nope', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        )
        .text;
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
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session
        .ral(
            "exarch-schedules `add [trigger: `bogus 'x', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        )
        .text;
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
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session
        .ral(
            "exarch-schedules `add [trigger: `after '1s', label: 'nightly']",
            5,
            &emit,
        )
        .text;
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
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session.ral("exarch-schedules `add [trigger: `after '1s', label: 'nightly', prompt: #'wake'#, extra: 1]",
        5,
        &emit,
    ).text;
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
    let session = granted_trunk();

    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    let result = session
        .ral(
            "exarch-schedules `add [trigger: `after '10m', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        )
        .text;
    assert!(
        result.contains("nightly"),
        "the table answered afterwards must carry the row just armed, got: {result}"
    );
}

/// The wait is generous because the fire really is a wall-clock second
/// away: `parse_duration`'s smallest unit is whole seconds.
#[test]
fn schedule_full_stack_round_trip_answers_the_table_and_fires_into_inbox() {
    let session = granted_trunk();

    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    let result = session
        .ral(
            "exarch-schedules `add [trigger: `after '1s', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        )
        .text;
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
            Some(crate::bus::Next::Item(crate::bus::Item::Wakeup(text))) => {
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
    let session = granted_trunk();

    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    let result = session
        .ral(
            "exarch-schedules `add [trigger: `after '10m', label: 'nightly', prompt: #'wake'#]",
            5,
            &emit,
        )
        .text;
    assert!(
        result.contains("EXIT: 0"),
        "a valid exarch-schedules `add call must succeed, got: {result}"
    );
    assert_eq!(
        session.agent.schedules.list().len(),
        1,
        "the schedule must be registered"
    );

    let result = session
        .ral("exarch-schedules `remove 'nightly'", 5, &emit)
        .text;
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

    let miss = session
        .ral("exarch-schedules `remove 'nightly'", 5, &emit)
        .text;
    assert!(
        miss.contains("EXIT: 0"),
        "removing an already-absent label answers the same empty table, not an error, got: {miss}"
    );
}

/// A single armed schedule cannot tell "the removed row is gone" apart
/// from "the table is empty": two distinguishable labels can.
#[test]
fn schedule_remove_answer_omits_the_removed_row_but_keeps_the_other() {
    let session = granted_trunk();

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

    let result = session
        .ral("exarch-schedules `remove 'nightly'", 5, &emit)
        .text;
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
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
        "test-model",
        crate::provider::scripted::Script::new().then(
            crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                "c1",
                r#"let found = ["a.rs", "b.rs"]; exarch-agents `reply [files: $found]"#,
            )]),
        ),
    ));
    session.agent.provider.swap(provider);
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    let result = session.ral(r"exarch-agents `start [prompt: #'find files'#, name: 'finder', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
        5,
        &emit,
    ).text;
    assert!(
        result.contains("live: 1"),
        "the summary must be the run's value and must count the child, got: {result}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match session.next_item_for_test() {
            Some(crate::bus::Next::Item(crate::bus::Item::Agent(_))) => break,
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

    let read = session.ral(r"exarch-agents `read 'finder'", 5, &emit).text;
    assert!(
        read.contains("files:") && read.contains("a.rs") && read.contains("b.rs"),
        "the structured record must reach the parent through `exarch-agents `read`, got: {read}"
    );
}

/// A block in the argument is refused statically (`data`, T0074), and the
/// refusal is an ordinary error, not a termination: a later, well-formed
/// `` exarch-agents `reply `` still succeeds.  The door's own `first_order`
/// check stays as the conversion it is — `FOValue::try_from` is fallible —
/// but no ral program reaches it with a block.
#[test]
fn reply_refuses_a_non_first_order_value_and_does_not_terminate() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    let result = session
        .ral(r"exarch-agents `reply { echo hi }", 5, &emit)
        .text;
    assert!(
        result.contains("T0074") && result.contains("data"),
        "must be refused statically as non-data, got: {result}"
    );

    let ok = session.ral(r"exarch-agents `reply 42", 5, &emit).text;
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
    session.agent.provider.swap(provider);
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let provider_handle = session.agent.current_provider();
    let outcome = session.deliberate(&provider_handle, Some("go".into()), None, &emit);
    match outcome {
        Ok(crate::agent::deliberate::Outcome::Replied) => {
            let v = session.agent.reply().expect("the reply is deposited");
            assert_eq!(
                v,
                ral_core::first_order::FOValue::String {
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
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
        "test-model",
        crate::provider::scripted::Script::new().then(
            crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                "c1",
                r#"exarch-pins `set [key: "note", body: `card ["hi there"]]; exarch-agents `reply !{exarch-pins `read "note"}"#,
            )]),
        ),
    ));
    session.agent.provider.swap(provider);
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    let result = session.ral(r"exarch-agents `start [prompt: #'pin and read back'#, name: 'pinner', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
        5,
        &emit,
    ).text;
    assert!(
        result.contains("live: 1"),
        "the summary must be the run's value and must count the child, got: {result}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match session.next_item_for_test() {
            Some(crate::bus::Next::Item(crate::bus::Item::Agent(r))) => {
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
    let read = session.ral(r"exarch-agents `read 'pinner'", 5, &emit).text;
    assert!(
        read.contains("`card") && read.contains("`text [spans:"),
        "the canonical card must reach the parent, got: {read}"
    );
}

/// A child's reply is checked where the parent reads it: a field the
/// script projects and the reply lacks fails at the `` `read ``, naming
/// the pointer, and not at a later step that would use it.
#[test]
fn a_reply_missing_a_projected_field_is_refused_at_the_read() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
        "test-model",
        crate::provider::scripted::Script::new().then(
            crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                "c1",
                r"exarch-agents `reply [note: 'all fine']",
            )]),
        ),
    ));
    session.agent.provider.swap(provider);
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    session.ral(r"exarch-agents `start [prompt: #'reply without a verdict'#, name: 'lazy', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
        5,
        &emit,
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !matches!(
        session.next_item_for_test(),
        Some(crate::bus::Next::Item(crate::bus::Item::Agent(_)))
    ) {
        assert!(
            std::time::Instant::now() < deadline,
            "child did not settle within the timeout"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let result = session
        .ral(
            "let r = exarch-agents `read 'lazy'\necho $r[reply][verdict]",
            5,
            &emit,
        )
        .text;
    assert!(
        result.contains("exarch-agents: the value at `/reply` has no field `verdict`"),
        "the read must refuse a reply missing what the script projects, got: {result}"
    );
}

/// An absent key answers `` `none ``, which crosses to the parent as the
/// reply.
#[test]
fn pin_read_full_stack_absent_key_replies_none() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let provider = std::sync::Arc::new(crate::provider::Provider::scripted(
        "test-model",
        crate::provider::scripted::Script::new().then(
            crate::provider::scripted::Reply::tool_calls(vec![ral_call(
                "c1",
                r#"exarch-agents `reply !{exarch-pins `read "nope"}"#,
            )]),
        ),
    ));
    session.agent.provider.swap(provider);
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    let result = session.ral(r"exarch-agents `start [prompt: #'read an absent key'#, name: 'reader', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
        5,
        &emit,
    ).text;
    assert!(
        result.contains("live: 1"),
        "the summary must be the run's value and must count the child, got: {result}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match session.next_item_for_test() {
            Some(crate::bus::Next::Item(crate::bus::Item::Agent(r))) => {
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

    let read = session.ral(r"exarch-agents `read 'reader'", 5, &emit).text;
    assert!(
        read.contains("reply: `none"),
        "an absent key must reply `none, got: {read}"
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

    let session = crate::agent::Avatar::for_test("system").unwrap();
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

    let listed = session.ral("exarch-tasks `list", BUDGET, &emit).text;
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

    let read = session
        .ral(
            r#"let [decoded-task, _] = !{tasks-decode !{exarch-pins `read "tasks"}}
           echo $decoded-task[desc]
           echo $decoded-task[status]
           echo !{intercalate "," $decoded-task[tags]}
           echo $decoded-task[notes]"#,
            BUDGET,
            &emit,
        )
        .text;
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
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    session.ral(
        r#"let f = { exarch-tasks `add "inside a block" }; !{f}"#,
        5,
        &emit,
    );

    let listed = session.ral("exarch-tasks `list", 5, &emit).text;
    assert!(
        listed.contains("inside a block"),
        "a task added inside a function body must survive to the next top-level run, got: {listed}"
    );
}

/// A sub-agent's register is its own: a child's `` exarch-tasks `add `` must never
/// reach the parent's "tasks" pin.
#[test]
fn sub_agent_pinning_tasks_leaves_the_parents_register_untouched() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
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
    session.agent.provider.swap(provider);

    let result = session.ral(r"exarch-agents `start [prompt: #'add a task'#, name: 'tasker', type: `amnemon, grant: `read-only, search: false, provider: `inherit, model: `inherit]",
        5,
        &emit,
    ).text;
    assert!(
        result.contains("live: 1"),
        "the summary must be the run's value and must count the child, got: {result}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match session.next_item_for_test() {
            Some(crate::bus::Next::Item(crate::bus::Item::Agent(_))) => break,
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

    let listed = session.ral("exarch-tasks `list", 5, &emit).text;
    assert!(
        listed.contains("parent task"),
        "the parent's own task must survive, got: {listed}"
    );
    assert!(
        !listed.contains("child task"),
        "the child's pin must never reach the parent's register, got: {listed}"
    );
}

/// `` exarch-tasks `load `` decodes a file inside a stored function, so its
/// decoder's site lives in the library's own IR: a list saved and loaded
/// back passes that site, statuses and tags included.
#[test]
fn a_saved_task_list_loads_back_through_its_decoder_site() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let path = std::env::temp_dir().join(format!("tasks-{}.json", std::process::id()));

    session.ral(r#"exarch-tasks `add "keep me""#, 5, &emit);
    session.ral(r#"exarch-tasks `tag [id: 1, tag: "urgent"]"#, 5, &emit);
    session.ral("exarch-tasks `status [id: 1, status: `doing]", 5, &emit);
    let saved = session
        .ral(
            &format!("exarch-tasks `save '{}'", path.display()),
            5,
            &emit,
        )
        .text;
    assert!(
        !saved.contains("denied"),
        "the save must land, got: {saved}"
    );
    session.ral(r#"exarch-pins `clear "tasks""#, 5, &emit);
    let loaded = session
        .ral(
            &format!("exarch-tasks `load '{}'", path.display()),
            5,
            &emit,
        )
        .text;
    let listed = session.ral("exarch-tasks `list", 5, &emit).text;
    let _ = std::fs::remove_file(&path);
    assert!(
        !loaded.contains("the value"),
        "the decode must admit what save wrote, got: {loaded}"
    );
    for field in ["keep me", "urgent", "`doing"] {
        assert!(listed.contains(field), "missing {field} in: {listed}");
    }
}

/// `tasks-sync` clears the slot once no work remains: transitioning the
/// last open task to `` `done `` empties the pin, and a later `` exarch-tasks `add ``
/// finds no register and restarts id allocation at 1.
#[test]
fn transitioning_the_last_open_task_to_done_clears_the_pin_and_restarts_ids() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    session.ral(r#"exarch-tasks `add "only task""#, 5, &emit);
    session.ral("exarch-tasks `status [id: 1, status: `done]", 5, &emit);

    let read = session.ral(r#"exarch-pins `read "tasks""#, 5, &emit).text;
    assert!(
        read.contains("`none") && !read.contains("`some"),
        "an all-done list must clear the pin to `none, got: {read}"
    );

    session.ral(r#"exarch-tasks `add "fresh""#, 5, &emit);
    let listed = session.ral("exarch-tasks `list", 5, &emit).text;
    for field in ["id: 1", "fresh", "`open"] {
        assert!(
            listed.contains(field),
            "id allocation must restart at 1 once the register is empty, missing {field} in: {listed}"
        );
    }
}

/// A card under "tasks" that `tasks-decode` does not recognise: the
/// model scribbled on the shared key — fails the next kit call with the
/// didactic message naming the expected shape, rather than corrupting or
/// silently discarding it.
#[test]
fn a_foreign_card_under_tasks_fails_the_kit_didactically() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    session.ral(r#"exarch-pins `set [key: "tasks", body: `card [`text [spans: [[text: "not task shaped"]]]]]"#,
        5,
        &emit,
    );

    let result = session.ral(r#"exarch-tasks `add "x""#, 5, &emit).text;
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
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session.ral("exarch-context `rewind [3]", 5, &emit).text;
    for tag in ["survey", "evict"] {
        assert!(result.contains(tag), "must name `{tag}, got: {result}");
    }
}

/// `` `evict ``'s record row is open only on the tail, so `turns` itself
/// is still static: a misspelling reaches the type error, not the door.
#[test]
fn misspelled_evict_field_errors_statically_naming_the_field() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session
        .ral("exarch-context `evict [turn: [1], note: `none]", 5, &emit)
        .text;
    assert!(
        !result.contains("EXIT: 0") && result.contains("turns"),
        "the diagnostic must name the field the row demands, got: {result}"
    );
}

/// Absence is a variant, so the calls that leave a key out or pass a bare
/// value in its place are type errors naming the key.
#[test]
fn absence_is_spelled_with_its_variant() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    for (call, key) in [
        ("exarch-context `evict [turns: [1]]", "note"),
        ("exarch-context `evict [turns: [1], note: 'x']", "note"),
        ("exarch-transcript `grep [pattern: 'x']", "turns"),
        (
            "exarch-transcript `grep [pattern: 'x', turns: [1]]",
            "turns",
        ),
    ] {
        let result = session.ral(call, 5, &emit).text;
        assert!(
            !result.contains("EXIT: 0") && result.contains(key),
            "`{call}` must be refused naming `{key}`, got: {result}"
        );
    }
}

/// The whole family through the real shell: an edit answers the survey
/// the transition leaves behind, so the count of what left is there to
/// read without a second call. The address names turns — 1 and 2 are the
/// first prompt and its reply — and `note` is a variant, so both of its
/// shapes type-check.
#[test]
fn an_eviction_answers_the_survey_it_leaves_behind() {
    let session = trunk_with_an_answered_prompt();
    crate::agent::testkit::close_exchange(&session, "second prompt", "second answer");
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    let result = session
        .ral(
            "exarch-context `evict [turns: [1, 2], note: `some 'the old work is done']",
            5,
            &emit,
        )
        .text;
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

/// The same tag with `none` for its note: the harness never asks the model
/// for an empty string.
#[test]
fn an_eviction_without_a_note_type_checks() {
    let session = trunk_with_an_answered_prompt();
    crate::agent::testkit::close_exchange(&session, "second prompt", "second answer");
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    let result = session
        .ral(
            "exarch-context `evict [turns: !{range 1 3}, note: `none]",
            5,
            &emit,
        )
        .text;
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
    let session = trunk_with_an_answered_prompt();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    let result = session
        .ral(
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
        )
        .text;
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
    let session = trunk_with_an_answered_prompt();
    crate::agent::testkit::close_exchange(&session, "second prompt", "second answer");
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    let result = session
        .ral(
            r"let spanning = exarch-transcript `read [turns: [2, 3]]
           echo !{length $spanning}
           echo $spanning[0][turn] $spanning[0][role] !{length $spanning[0][messages]}
           echo $spanning[1][turn] $spanning[1][role]
           let built = exarch-transcript `read [turns: !{range 3 5}]
           echo !{length $built} $built[1][turn]",
            5,
            &emit,
        )
        .text;
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
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let result = session.ral("exarch-transcript `search 'x'", 5, &emit).text;
    for tag in ["index", "read", "grep"] {
        assert!(result.contains(tag), "must name `{tag}, got: {result}");
    }
}

/// The answer type is free, so each tag's own shape has to type-check
/// against the use the program makes of it: an index row carries `id` and
/// `held`, and a grep answer projects `hits` and `total` as a record, each
/// hit naming the turn it lies in. `turns` is `` `all `` or `` `only ``.
#[test]
fn index_and_grep_answer_their_own_shapes() {
    let session = trunk_with_an_answered_prompt();
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);

    let result = session
        .ral(
            r"let listed = exarch-transcript `index
           echo $listed[0][id] $listed[0][role] $listed[0][held]
           let matched = exarch-transcript `grep [pattern: 'first (prompt|answer)', turns: `all]
           echo !{length $matched[hits]} $matched[total]
           echo $matched[hits][0][turn] $matched[hits][1][turn]
           let narrowed = exarch-transcript `grep [pattern: 'first', turns: `only [2]]
           echo $narrowed[total]
           let missed = exarch-transcript `grep [pattern: 'nothing here', turns: `only [1, 2]]
           echo $missed[total]",
            5,
            &emit,
        )
        .text;
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
