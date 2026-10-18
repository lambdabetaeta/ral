//! The agents family: `start`, `cancel`, `message`, `read`, `reply`.

use super::*;

/// The child's reply notice lands in the parent's inbox once it settles, and
/// `` `read `` answers the very value it deposited.
#[test]
fn agent_start_spawns_and_delivers_result_to_parent_inbox() {
    let (desk, _fleet, parent_inbox) = spawnable_desk(3);
    let provider = Arc::new(Provider::scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![ral_call(
            "r1",
            "exarch-agents `reply 'hi from child'",
        )])),
    ));
    desk.services.agent.provider.swap(provider);

    let answer = with_parked(&desk, |desk, session| {
        desk.ask(start_req(session, "say hi", "helper"))
    })
    .expect("a valid `start must succeed");

    let (live, _) = summary_counts(&answer);
    assert_eq!(live, 1, "the child is the one other agent alive");

    match wait_for_settle(&parent_inbox) {
        crate::bus::Next::Item(crate::bus::Item::Agent(result)) => {
            assert!(
                matches!(result.outcome, crate::bus::AgentOutcome::Replied),
                "the child's reply notice must reach the parent's inbox, got: {:?}",
                result.outcome
            );
        }
        other => panic!("expected an Agent result item, got {other:?}"),
    }
    let answer = desk
        .ask(Request::Agents(Agents::Read("helper".into())))
        .expect("a descendant that has replied must answer its reply");
    assert_eq!(str_field(&answer, "name"), Some("helper"));
    assert_eq!(str_field(&answer, "reply"), Some("hi from child"));
}

/// The state *is* the answer, not a receipt about the child just started:
/// the roster a spawn answers with carries a sibling that spawn never
/// touched, so nothing has to ask again to see what the fleet now is.
#[test]
fn start_answers_the_fleets_state_not_a_receipt() {
    let (desk, fleet, parent_inbox) = spawnable_desk(3);
    desk.services
        .agent
        .provider
        .swap(Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "exarch-agents `reply 'a'",
            )])),
        )));
    // Held for the whole test, and with no worker behind it, so it never
    // settles out from under the assertion below.
    let mut sibling = crate::agent::testkit::TestAgentSpec::new("already-there");
    sibling.role = crate::agent::Role::child(&desk.services.agent);
    let _already_there =
        crate::agent::testkit::test_agent(&fleet, sibling).expect("a fresh child of a live parent");

    let (live, _) = summary_counts(
        &with_parked(&desk, |desk, session| {
            desk.ask(start_req(session, "go", "helper"))
        })
        .expect("the spawn must succeed"),
    );
    assert_eq!(
        live, 2,
        "the spawn counts the fleet's state, the sibling it did not start included"
    );
    let rows = listed(&desk);
    let mut names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["already-there", "helper", "parent"],
        "and `list names them, the reader among them"
    );

    let _ = wait_for_settle(&parent_inbox);
}

/// The parent's own pair, inherited or spelt out, is the parent's own
/// `Arc<Provider>`: nothing is allocated and the bureau is asked nothing.
#[test]
fn the_parents_own_pair_hands_the_child_the_parents_provider() {
    let desk = desk();
    let parent = desk.services.agent.current_provider();
    for model in [Selection::Inherit, Selection::Named(parent.model().into())] {
        let child = desk
            .child_provider(&Selection::Inherit, &model)
            .expect("the parent's own pair needs no minting");
        assert!(Arc::ptr_eq(&parent, &child));
    }
}

/// A scripted session mints nothing, so a spawn that names a selection is
/// refused saying so, before anything is adopted: no child is registered.
#[test]
fn a_refused_selection_never_registers_a_child() {
    let (desk, _fleet, _parent_inbox) = spawnable_desk(3);
    // Refused before any fork is taken up, so none need be parked.
    let err = desk
        .ask(start(
            ForkClaim::Parked(NurseryId(0)),
            Spec {
                model: Selection::Named("other-model".into()),
                ..spec("go", "helper")
            },
        ))
        .expect_err("a scripted bureau must refuse to mint");
    assert!(
        err.message.contains("`` exarch-agents `start `` refused")
            && err.message.contains("mints no others"),
        "the refusal must name the tag and the reason, got: {}",
        err.message
    );
    assert_eq!(
        summary(&desk.services.agent).live,
        0,
        "a refused selection must never register a child"
    );
}

/// A `` `start `` whose spec record `edit` has spoilt, as no door sends it.
fn spoilt_start(edit: impl FnOnce(&mut Vec<(String, FOValue)>)) -> FOValue {
    let mut spec = spec("go", "scout").encode();
    let FOValue::Map { entries } = &mut spec else {
        unreachable!("a spec is a record")
    };
    edit(entries);
    let fork = ForkClaim::Parked(NurseryId(0)).encode();
    family_req(
        "agents",
        "start",
        Some(FOValue::Map {
            entries: vec![("spec".into(), spec), ("fork".into(), fork)],
        }),
    )
}

/// The desk reads the model's record by field name, so a missing field is
/// named — never a position the model never wrote.
#[test]
fn start_refuses_a_spec_missing_a_field_by_name() {
    let err = desk()
        .handle(&spoilt_start(|spec| spec.retain(|(key, _)| key != "name")))
        .expect_err("a spec missing `name` must be refused");
    assert_eq!(
        err.message,
        "`` exarch-agents `start ``: `spec: no `name field in a record of 4 fields"
    );
}

/// A misspelt spec field is refused with the field it most likely meant.
#[test]
fn start_refuses_a_misspelt_spec_field() {
    let err = desk()
        .handle(&spoilt_start(|spec| spec[1].0 = "nmae".into()))
        .expect_err("`nmae` is not a field the spec carries");
    assert_eq!(
        err.message,
        "`` exarch-agents `start ``: `spec: unknown field `nmae`: did you mean `name`?"
    );
}

#[test]
fn agent_start_refuses_at_zero_fuel_with_the_exhaustion_text() {
    let (desk, _fleet, _parent_inbox) = spawnable_desk(0);
    let (err, left_parked) = with_parked(&desk, |desk, session| {
        let answer = desk.ask(start_req(session, "hi", "helper"));
        (answer, still_parked(desk, session))
    });
    let err = err.expect_err("zero fuel must refuse");
    assert!(
        err.message.contains("no spawn fuel remains"),
        "got: {}",
        err.message
    );
    assert!(
        err.message.contains("Fuel bounds how deep"),
        "must state that fuel bounds depth, not fan-out, got: {}",
        err.message
    );
    assert!(
        left_parked,
        "a fuel refusal happens before adopt, so the parked fork must \
         stay for the run guard to reap, never claimed by a refused call"
    );
}

/// [`Fleet::name_live`] catches the ordinary case before the parked
/// fork is ever adopted, so the refused spawn registers no child.
#[test]
fn agent_start_refuses_a_name_already_borne_by_a_live_agent() {
    let (desk, _fleet, _parent_inbox) = spawnable_desk(3);
    let provider = Arc::new(Provider::scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![ral_call(
            "r1",
            "exarch-agents `reply 'a'",
        )])),
    ));
    desk.services.agent.provider.swap(provider);

    let answer = with_parked(&desk, |desk, session| {
        desk.ask(start_req(session, "go", "helper"))
    });
    assert!(answer.is_ok(), "the first spawn must succeed");

    let (err, left_parked) = with_parked(&desk, |desk, session| {
        let answer = desk.ask(start_req(session, "go again", "helper"));
        (answer, still_parked(desk, session))
    });
    let err = err.expect_err("a second spawn naming a live agent must be refused");
    assert!(
        err.message.contains("already bears the name 'helper'"),
        "got: {}",
        err.message
    );
    assert!(
        left_parked,
        "a name-collision refusal happens before adopt, so the parked \
         fork must stay for the run guard to reap, never claimed by a \
         refused call"
    );
    assert_eq!(
        summary(&desk.services.agent).live,
        1,
        "the second, refused spawn must leave no child behind"
    );
}

/// The in-process door checks a name, but a wire peer need not have come
/// through one — so the spawn spine checks it too, before a log is forked
/// or a listener dialled, rather than leaving it all to the enrolment.
#[test]
fn agent_start_refuses_a_malformed_name_before_it_adopts_the_fork() {
    let (desk, _fleet, _parent_inbox) = spawnable_desk(3);

    let (err, left_parked) = with_parked(&desk, |desk, session| {
        let answer = desk.ask(start_req(session, "go", "help/er"));
        (answer, still_parked(desk, session))
    });
    let err = err.expect_err("a malformed name must be refused");
    assert!(
        err.message.contains("ASCII letters"),
        "the refusal must carry the name rule; got: {}",
        err.message
    );
    assert!(
        left_parked,
        "the name is refused before adopt, so the parked fork must stay \
         for the run guard to reap"
    );
    assert_eq!(
        summary(&desk.services.agent).live,
        0,
        "a refused spawn leaves no child behind"
    );
}

/// Siblings in one turn all succeed off the same captured fuel, since the
/// parent's own is never debited.
#[test]
fn fuel_bounds_depth_not_fanout() {
    let (desk, _fleet, parent_inbox) = spawnable_desk(1);
    let provider = Arc::new(Provider::scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call(
                "r1",
                "exarch-agents `reply 'a'",
            )]))
            .then(Reply::tool_calls(vec![ral_call(
                "r2",
                "exarch-agents `reply 'b'",
            )]))
            .then(Reply::tool_calls(vec![ral_call(
                "r3",
                "exarch-agents `reply 'c'",
            )])),
    ));
    desk.services.agent.provider.swap(provider);

    for i in 0..3 {
        let answer = with_parked(&desk, move |desk, session| {
            desk.ask(start_req(session, "go", &format!("t{i}")))
        });
        assert!(
            answer.is_ok(),
            "sibling {i} must not be refused for lack of fuel: fuel \
             bounds depth, not fan-out"
        );
    }

    for _ in 0..3 {
        match wait_for_settle(&parent_inbox) {
            crate::bus::Next::Item(crate::bus::Item::Agent(result)) => assert!(
                matches!(result.outcome, crate::bus::AgentOutcome::Replied),
                "every sibling must settle by replying, got: {:?}",
                result.outcome
            ),
            other => panic!("expected an Agent result item, got {other:?}"),
        }
    }
}

/// The capture went stale the instant the *calling* session's inbox
/// epoch moved past what was snapshotted at install.
#[test]
fn agent_start_refuses_after_clear() {
    let (desk, _fleet, parent_inbox) = spawnable_desk(3);
    parent_inbox.clear(); // the /clear gesture, on this caller
    let err = with_parked(&desk, |desk, session| {
        desk.ask(start_req(session, "hi", "helper"))
    })
    .expect_err("a stale epoch must refuse");
    assert!(
        err.message.contains("cleared"),
        "must name the /clear cause, got: {}",
        err.message
    );
}

/// A refusal at adoption: here, a restriction record the capability
/// decoder will not read, which only the engine's own freeze discovers:
/// drops the bequest rather than leaving it to be adopted twice.  The grant
/// rides the bequest the run parked; the desk reads none of its own.
#[test]
fn refused_enquiry_leaves_the_nursery_empty() {
    let (desk, _fleet, _parent_inbox) = spawnable_desk(3);
    let (answer, left_parked) = parked_by(
        &desk,
        r"exarch-agents `start [prompt: #'go'#, name: 'helper', type: `amnemon, grant: `restrict [net: 'yes'], provider: `inherit, model: `inherit]",
        |desk, session| {
            let answer = desk.ask(start_req(session, "go", "helper"));
            (answer, still_parked(desk, session))
        },
    );
    let err = answer.expect_err("a `net axis that is not a Bool must refuse");
    assert!(
        err.message.contains("net"),
        "must name the axis it could not read, got: {}",
        err.message
    );
    assert!(
        !left_parked,
        "a refusal downstream of adopt must not leave the bequest \
         re-adoptable: it was already claimed and simply drops"
    );
}

/// `` `cancel `` may only reach what this agent started; `` `message ``
/// reaches any live agent but this one.
#[test]
fn cancel_scopes_to_descendants_and_message_does_not() {
    let (services, fleet, _root_inbox) = services_with(3, crate::agent::fleet::Launch::for_test());
    let desk_root = ExarchDesk { services };
    // root -> mid -> grandchild, and root -> sibling (mid's sibling).
    let under = |name: &str, parent: &Arc<Agent>| {
        let mut spec = crate::agent::testkit::TestAgentSpec::new(name);
        spec.role = crate::agent::Role::child(parent);
        crate::agent::testkit::test_agent(&fleet, spec).expect("a fresh child of a live parent")
    };
    let root = desk_root.services.agent.clone();
    let mid = under("mid", &root);
    let _sibling = under("sibling", &root);
    let _grandchild = under("grandchild", &mid);

    let mut desk1 = desk_root;
    desk1.services.agent = mid;

    for who in ["sibling", "parent", "grandchild"] {
        assert!(
            desk1.ask(message_req(who, "hi")).is_ok(),
            "a message must reach {who}, whichever way across the tree it runs"
        );
    }

    let err = desk1
        .ask(message_req("mid", "hi"))
        .expect_err("a message to oneself must be refused");
    assert_eq!(
        err.message,
        "agent 'mid' is you; `` exarch-agents `message `` reaches another agent: to wake yourself, arm a `exarch-schedules` fire"
    );

    let cancel_err = desk1
        .ask(Request::Agents(Agents::Cancel("parent".into())))
        .expect_err("cancelling an ancestor must be refused");
    assert_eq!(
        cancel_err.message,
        "agent 'parent' is not an agent you started; `` exarch-agents `cancel `` may only reach a descendant of yours"
    );

    assert!(
        desk1
            .ask(Request::Agents(Agents::Cancel("grandchild".into())))
            .is_ok(),
        "cancelling a proper descendant must succeed"
    );
}

/// The read-after-write law once more, through a real spawn: a surfaced
/// value must render before an enquiry raised after it in the same run.
#[test]
fn surface_then_spawn_observes_the_surface_first() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, rx) = channel();
    let emit = Emitter::new(tx, session.agent.id);
    let _ = session.ral(r#"exarch-pins `clear "test-marker"; exarch-agents `start [prompt: #'go'#, name: 't', type: `amnemon, grant: `confined, provider: `inherit, model: `inherit]"#,
        5,
        &emit,
    );
    drop(emit);

    let mut saw_unpin = false;
    let mut saw_spawn = false;
    while let Ok(sig) = rx.try_recv() {
        match sig {
            Signal::Transient(_, Transient::Unpin { .. }) => saw_unpin = true,
            Signal::Fact(_, fact) => {
                if let Record::Display(Display::HarnessCall { verb, .. }) = fact.value()
                    && verb == "spawn"
                {
                    assert!(
                        saw_unpin,
                        "the surfaced unpin must be observed before the spawn's HarnessCall line"
                    );
                    saw_spawn = true;
                }
            }
            Signal::Transient(..) => {}
        }
    }
    assert!(
        saw_spawn,
        "the agent spawn's HarnessCall chrome must have been emitted"
    );
}

/// Each call also puts a subject-less act on the rail, carrying its value.
#[test]
fn reply_stages_the_payload_last_write_wins() {
    let (tx, rx) = channel();
    let mut d = desk();
    d.services
        .log
        .borrow_mut()
        .record_emitter()
        .attach(Box::new(FleetSink {
            id: AgentId::new(0),
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        }));
    d.services.emit = Emitter::new(tx, AgentId::new(0));

    for text in ["first", "second"] {
        d.ask(reply_req(FOValue::String { value: text.into() }))
            .expect("a returning agent's reply must succeed");
    }

    assert_eq!(
        d.services.reply.borrow_mut().take(),
        Some(FOValue::String {
            value: "second".into()
        }),
        "the last staged reply wins"
    );

    let acts: Vec<String> = crate::bus::drain_records(&rx)
        .into_iter()
        .filter_map(|rec| match rec {
            Record::Display(Display::HarnessCall {
                verb,
                subject,
                payload,
                failed,
            }) if verb == "reply" => {
                assert_eq!(subject, None, "`reply` addresses no named subject");
                assert!(!failed, "a staged reply is not a refusal");
                Some(payload)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        acts,
        ["first", "second"],
        "each reply emits its own act row, carrying the value it staged"
    );
}
