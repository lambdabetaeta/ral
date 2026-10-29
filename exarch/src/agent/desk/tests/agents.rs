//! The agents family: spawn, status reads, Stop and Dismiss.

use super::*;
use crate::agent::nursery::{Joined, Teardown};
use crate::agent::testkit::{TestAgentSpec, test_agent};

/// The report is a message; read answers the helper's published status.
#[test]
fn agent_start_reports_to_the_parent_mailbox_and_publishes_the_read_value() {
    with_spawnable_desk(3, |desk, _fleet, agent| {
        let parent_mailbox = &agent.mailbox;
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "exarch-agents `reply 'hi from child'",
            )])),
        ));
        desk.call.handle.0.provider.swap(provider);

        let answer = with_parked(&desk, |desk, session| {
            desk.ask(start_req(session, "say hi", "helper"))
        })
        .expect("a valid `start must succeed");

        let (live, _) = summary_counts(&answer);
        assert_eq!(live, 1, "the child is the one other agent alive");

        match next_input(parent_mailbox) {
            Input::Child {
                status: ChildStatus::Replied(value),
                ..
            } => {
                assert_eq!(value, text("hi from child"));
            }
            _ => panic!("the helper must report its reply to its parent"),
        }
        wait_at_rest(&desk.call.fleet.resolve("helper").unwrap());
        let answer = desk
            .ask(Request::Agents(Agents::Read("helper".into())))
            .expect("a descendant that has replied must answer its reply");
        assert_eq!(str_field(&answer, "name"), Some("helper"));
        assert_eq!(str_field(&answer, "reply"), Some("hi from child"));
    });
}

fn adopt_synthetic(desk: &ExarchDesk, helper: &TestAgent) {
    let thread = Joined::new(std::thread::spawn(|| Teardown {
        failures: Vec::new(),
    }));
    desk.loan
        .get()
        .unwrap()
        .state
        .nursery
        .adopt((**helper).clone(), thread);
}

#[test]
fn read_answers_the_published_reply_even_after_the_helper_ends() {
    with_spawnable_desk(3, |desk, fleet, agent| {
        let child = test_agent(
            &fleet,
            TestAgentSpec::child("helper", &agent, agent.mailbox.life().child()),
        )
        .unwrap();
        adopt_synthetic(&desk, &child);
        child.report(ChildStatus::Replied(text("hi from child")));
        for ended in [false, true] {
            if ended {
                child.end();
            }
            let answer = desk
                .ask(Request::Agents(Agents::Read("helper".into())))
                .unwrap();
            assert_eq!(str_field(&answer, "name"), Some("helper"));
            assert_eq!(str_field(&answer, "reply"), Some("hi from child"));
        }
        drop(child);
        assert!(fleet.resolve("helper").is_none());
        assert_eq!(
            str_field(
                &desk
                    .ask(Request::Agents(Agents::Read("helper".into())))
                    .unwrap(),
                "reply"
            ),
            Some("hi from child")
        );
    });
}

#[test]
fn read_distinguishes_working_failed_stopped_and_unowned_helpers() {
    with_spawnable_desk(3, |desk, fleet, agent| {
        let child = test_agent(
            &fleet,
            TestAgentSpec::child("helper", &agent, agent.mailbox.life().child()),
        )
        .unwrap();
        adopt_synthetic(&desk, &child);
        let grandchild = test_agent(
            &fleet,
            TestAgentSpec::child("grandchild", &child, child.mailbox.life().child()),
        )
        .unwrap();
        grandchild.report(ChildStatus::Replied(text("not yours")));
        assert!(
            desk.agent_read("helper".into())
                .unwrap_err()
                .message
                .contains("still working")
        );
        assert!(
            desk.agent_read("grandchild".into())
                .unwrap_err()
                .message
                .contains("reads only your own helpers")
        );
        child.report(ChildStatus::Failed("engine failed".into()));
        assert!(
            desk.agent_read("helper".into())
                .unwrap_err()
                .message
                .contains("failed without a reply to read: engine failed")
        );
        let silent = test_agent(
            &fleet,
            TestAgentSpec::child("silent", &agent, agent.mailbox.life().child()),
        )
        .unwrap();
        adopt_synthetic(&desk, &silent);
        silent.end();
        assert!(
            desk.agent_read("silent".into())
                .unwrap_err()
                .message
                .contains("has no reply to read")
        );
        child.report(ChildStatus::Stopped);
        assert!(
            desk.agent_read("helper".into())
                .unwrap_err()
                .message
                .contains("stopped without a reply to read; message it to continue")
        );
    });
}

#[test]
fn read_refuses_an_old_reply_while_a_follow_up_is_working() {
    with_spawnable_desk(3, |desk, fleet, agent| {
        let child = test_agent(
            &fleet,
            TestAgentSpec::child("helper", &agent, agent.mailbox.life().child()),
        )
        .unwrap();
        adopt_synthetic(&desk, &child);
        child.report(ChildStatus::Replied(text("old answer")));
        assert!(desk.agent_read("helper".into()).is_ok());
        child.message(&agent, "follow up".into()).unwrap();
        assert!(
            desk.agent_read("helper".into())
                .unwrap_err()
                .message
                .contains("still working")
        );
    });
}

/// The state *is* the answer, not a receipt about the child just started:
/// the roster a spawn answers with carries a sibling that spawn never
/// touched, so nothing has to ask again to see what the fleet now is.
#[test]
fn start_answers_the_fleets_state_not_a_receipt() {
    with_spawnable_desk(3, |desk, fleet, agent| {
        let parent_mailbox = &agent.mailbox;
        desk.call
            .handle
            .0
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
        let sibling = crate::agent::testkit::TestAgentSpec::child(
            "already-there",
            &desk.call.handle,
            agent.mailbox.life().child(),
        );
        let _already_there = crate::agent::testkit::test_agent(&fleet, sibling)
            .expect("a fresh child of a live parent");

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

        assert!(matches!(
            next_input(parent_mailbox),
            Input::Child {
                status: ChildStatus::Replied(_),
                ..
            }
        ));
    });
}

/// `Inherit` is the parent's own `Arc<Provider>`, and a configured role the
/// very `Arc` the human bound.
#[test]
fn inherit_is_the_parents_provider_and_a_role_the_bound_one() {
    let bound = Arc::new(Provider::scripted("role-model", Script::new()));
    let roles = crate::provider::roles::Roles(std::collections::BTreeMap::from([(
        "cheap".to_string(),
        Arc::clone(&bound),
    )]));
    let launch = crate::agent::fleet::Launch {
        roles,
        ..crate::agent::fleet::Launch::for_test()
    };
    with_fleet_desk(3, launch, |desk, _, _agent| {
        let parent = desk.call.handle.current_provider();
        let inherited = desk.child_provider(&Selection::Inherit).unwrap();
        assert!(Arc::ptr_eq(&parent, &inherited));
        let child = desk
            .child_provider(&Selection::Role("cheap".into()))
            .unwrap();
        assert!(Arc::ptr_eq(&bound, &child));
    });
}

/// A role no one bound, or none bound at all, is refused before anything is
/// adopted: no child is registered.
#[test]
fn a_refused_role_never_registers_a_child() {
    let bound = Arc::new(Provider::scripted("role-model", Script::new()));
    let configured = crate::agent::fleet::Launch {
        roles: crate::provider::roles::Roles(std::collections::BTreeMap::from([(
            "cheap".to_string(),
            bound,
        )])),
        ..crate::agent::fleet::Launch::for_test()
    };
    for (launch, said) in [
        (
            crate::agent::fleet::Launch::for_test(),
            "no roles are configured, so every child runs on your own model",
        ),
        (
            configured,
            "no role 'other': the configured roles are cheap; or pass model: `inherit",
        ),
    ] {
        with_fleet_desk(3, launch, |desk, _, _agent| {
            // Refused before any fork is taken up, so none need be parked.
            let err = desk
                .ask(start(
                    ForkClaim::Parked(NurseryId(0)),
                    Spec {
                        model: Selection::Role("other".into()),
                        ..spec("go", "helper")
                    },
                ))
                .expect_err("an unbound role must be refused");
            assert!(
                err.message.contains("`` exarch-agents `start `` refused")
                    && err.message.contains(said),
                "the refusal must name the tag and the reason, got: {}",
                err.message
            );
            assert_eq!(
                summary(&desk.call.handle, &desk.call.fleet).live,
                0,
                "a refused role must never register a child"
            );
        });
    }
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
    with_desk(|desk, _agent| {
        let err = desk
            .handle(&spoilt_start(|spec| spec.retain(|(key, _)| key != "name")))
            .expect_err("a spec missing `name` must be refused");
        assert_eq!(
            err.message,
            "`` exarch-agents `start ``: `spec: no `name field in a record of 3 fields"
        );
    });
}

/// A misspelt spec field is refused with the field it most likely meant.
#[test]
fn start_refuses_a_misspelt_spec_field() {
    with_desk(|desk, _agent| {
        let err = desk
            .handle(&spoilt_start(|spec| spec[1].0 = "nmae".into()))
            .expect_err("`nmae` is not a field the spec carries");
        assert_eq!(
            err.message,
            "`` exarch-agents `start ``: `spec: unknown field `nmae`: did you mean `name`?"
        );
    });
}

#[test]
fn agent_start_refuses_at_zero_fuel_with_the_exhaustion_text() {
    with_spawnable_desk(0, |desk, _fleet, _agent| {
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
    });
}

/// [`Fleet::name`] refuses the name before the parked fork is ever
/// adopted, so the refused spawn registers no child.
#[test]
fn agent_start_refuses_a_name_already_borne_by_a_live_agent() {
    with_spawnable_desk(3, |desk, _fleet, _agent| {
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "exarch-agents `reply 'a'",
            )])),
        ));
        desk.call.handle.0.provider.swap(provider);

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
            summary(&desk.call.handle, &desk.call.fleet).live,
            1,
            "the second, refused spawn must leave no child behind"
        );
    });
}

/// The in-process door checks a name, but a wire peer need not have come
/// through one — so the spawn spine checks it too, before a log is forked
/// or a listener dialled, rather than leaving it all to the enrolment.
#[test]
fn agent_start_refuses_a_malformed_name_before_it_adopts_the_fork() {
    with_spawnable_desk(3, |desk, _fleet, _agent| {
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
            summary(&desk.call.handle, &desk.call.fleet).live,
            0,
            "a refused spawn leaves no child behind"
        );
    });
}

/// Siblings in one turn all succeed off the same captured fuel, since the
/// parent's own is never debited.
#[test]
fn fuel_bounds_depth_not_fanout() {
    with_spawnable_desk(1, |desk, _fleet, agent| {
        let parent_mailbox = &agent.mailbox;
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
        desk.call.handle.0.provider.swap(provider);

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
            assert!(matches!(
                next_input(parent_mailbox),
                Input::Child {
                    status: ChildStatus::Replied(_),
                    ..
                }
            ));
        }
    });
}

#[test]
fn a_retained_desk_refuses_start_after_its_loan_returns() {
    let retained = with_spawnable_desk(3, |desk, _, _| desk);
    let err = retained
        .ask(start_req(NurseryId(0), "hi", "helper"))
        .unwrap_err();
    assert_eq!(err.code(), EnquiryError::no_desk().status);
    assert_eq!(err.message, EnquiryError::no_desk().message);
}

/// A refusal at adoption: here, a restriction record the capability
/// decoder will not read, which only the engine's own freeze discovers:
/// drops the bequest rather than leaving it to be adopted twice.  The grant
/// rides the bequest the run parked; the desk reads none of its own.
#[test]
fn refused_enquiry_leaves_the_nursery_empty() {
    with_spawnable_desk(3, |desk, _fleet, _agent| {
        let (answer, left_parked) = parked_by(
            &desk,
            r"exarch-agents `start [prompt: #'go'#, name: 'helper', type: `amnemon, grant: `restrict [net: 'yes'], model: `inherit]",
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
    });
}

#[test]
fn stop_scopes_to_descendants_without_ending_their_lives_or_helpers() {
    with_fleet_desk(
        3,
        crate::agent::fleet::Launch::for_test(),
        |mut desk, fleet, root| {
            let mid = test_agent(
                &fleet,
                TestAgentSpec::child("mid", &root, root.mailbox.life().child()),
            )
            .unwrap();
            let grandchild = test_agent(
                &fleet,
                TestAgentSpec::child("grandchild", &mid, mid.mailbox.life().child()),
            )
            .unwrap();
            let sibling = test_agent(
                &fleet,
                TestAgentSpec::child("sibling", &root, root.mailbox.life().child()),
            )
            .unwrap();
            let great_grandchild = test_agent(
                &fleet,
                TestAgentSpec::child(
                    "great-grandchild",
                    &grandchild,
                    grandchild.mailbox.life().child(),
                ),
            )
            .unwrap();
            desk.call.handle = (*mid).clone();
            desk.call.life = mid.mailbox.life();
            assert!(
                desk.agent_stop("grandchild").is_ok(),
                "Stop at rest is harmless"
            );
            assert!(
                desk.agent_stop("parent")
                    .unwrap_err()
                    .message
                    .contains("not an agent you started")
            );
            assert!(desk.agent_stop("sibling").is_err());
            assert!(desk.agent_stop("mid").is_err());
            grandchild.steer("go".into()).unwrap();
            let Next::Take { exchange, .. } = grandchild.mailbox.next(&Rest {
                owing: false,
                lives: Vec::new(),
                schedules: false,
            }) else {
                panic!("a human message resumes the descendant")
            };
            assert!(grandchild.mailbox.begin());
            grandchild.steer("human follow-up".into()).unwrap();
            desk.agent_stop("grandchild").unwrap();
            assert!(exchange.is_cancelled());
            assert_eq!(grandchild.queued(), ["human follow-up"]);
            for helper in [&root, &mid, &sibling, &grandchild, &great_grandchild] {
                assert!(helper.is_live());
            }
            assert!(desk.agent_stop("missing").is_ok());
        },
    );
}

/// The texts the desk's acts committed to the parent's record, in order.
fn harness_results(desk: &ExarchDesk) -> Vec<String> {
    let body = desk.loan.get().unwrap();
    crate::record::read_records(&body.log.dir().path().join(crate::record::RECORD))
        .unwrap()
        .into_iter()
        .filter_map(|record| match record {
            Record::Forensic(crate::record::Forensic::HarnessResult { text }) => Some(text),
            _ => None,
        })
        .collect()
}

/// A synthetic helper adopted into `desk`'s nursery, whose thread returns
/// `failures` once its life ends.
fn adopt_failing(
    desk: &ExarchDesk,
    helper: TestAgent,
    failures: Vec<String>,
) -> crate::agent::AgentHandle {
    let handle = (*helper).clone();
    desk.loan.get().unwrap().state.nursery.adopt(
        handle.clone(),
        Joined::new(std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while helper.is_live() {
                assert!(Instant::now() < deadline, "the helper was never ended");
                std::thread::sleep(Duration::from_millis(2));
            }
            drop(helper);
            Teardown { failures }
        })),
    );
    handle
}

#[test]
fn stop_answers_say_what_they_did_or_why_they_refused() {
    with_fleet_desk(
        3,
        crate::agent::fleet::Launch::for_test(),
        |mut desk, fleet, root| {
            let mid = test_agent(
                &fleet,
                TestAgentSpec::child("mid", &root, root.mailbox.life().child()),
            )
            .unwrap();
            let grandchild = test_agent(
                &fleet,
                TestAgentSpec::child("grandchild", &mid, mid.mailbox.life().child()),
            )
            .unwrap();
            let sibling = test_agent(
                &fleet,
                TestAgentSpec::child("sibling", &root, root.mailbox.life().child()),
            )
            .unwrap();
            desk.call.handle = (*mid).clone();
            desk.call.life = mid.mailbox.life();
            for refused in ["sibling", "mid"] {
                let message = desk.agent_stop(refused).unwrap_err().message;
                assert!(
                    message.contains(&format!("agent '{refused}' is not an agent you started"))
                        && message.contains("may only reach a descendant of yours"),
                    "{message}"
                );
            }
            desk.agent_stop("grandchild").unwrap();
            assert!(grandchild.mailbox.begin());
            desk.agent_stop("grandchild").unwrap();
            assert_eq!(
                harness_results(&desk)[2..],
                [
                    "agent 'grandchild' is at rest",
                    "stopped agent 'grandchild': its current exchange is cancelled; its own helpers work on",
                ]
            );
            assert!(sibling.is_live());
        },
    );
}

#[test]
fn dismiss_propagates_the_helpers_teardown_failures_to_its_parent() {
    with_spawnable_desk(3, |desk, fleet, root| {
        let helper = test_agent(
            &fleet,
            TestAgentSpec::child("helper", &root, root.mailbox.life().child()),
        )
        .unwrap();
        let helper = adopt_failing(&desk, helper, vec!["helper's teardown failure".into()]);
        desk.ask(Request::Agents(Agents::Dismiss("helper".into())))
            .unwrap();
        assert_eq!(harness_results(&desk), ["agent 'helper' dismissed"]);
        assert_eq!(
            desk.loan.get().unwrap().state.failures,
            ["helper's teardown failure"]
        );
        assert!(!helper.is_live());
        assert!(fleet.resolve("helper").is_none());
    });
}

#[test]
fn dismissing_an_already_joined_helper_says_so() {
    with_spawnable_desk(3, |desk, fleet, root| {
        let helper = test_agent(
            &fleet,
            TestAgentSpec::child("helper", &root, root.mailbox.life().child()),
        )
        .unwrap();
        let helper = adopt_failing(&desk, helper, Vec::new());
        helper.end();
        desk.loan.get().unwrap().state.nursery.join(helper.id());
        desk.ask(Request::Agents(Agents::Dismiss("helper".into())))
            .unwrap();
        assert_eq!(harness_results(&desk), ["agent 'helper' had already ended"]);
    });
}

#[test]
fn message_reaches_a_working_peer_but_a_resting_helper_admits_only_its_parent() {
    with_fleet_desk(
        3,
        crate::agent::fleet::Launch::for_test(),
        |mut desk, fleet, root| {
            let mid = test_agent(
                &fleet,
                TestAgentSpec::child("mid", &root, root.mailbox.life().child()),
            )
            .unwrap();
            let sibling = test_agent(
                &fleet,
                TestAgentSpec::child("sibling", &root, root.mailbox.life().child()),
            )
            .unwrap();
            desk.call.handle = (*mid).clone();
            desk.call.life = mid.mailbox.life();
            assert!(desk.ask(message_req("parent", "hi")).is_ok());
            assert!(
                desk.ask(message_req("mid", "hi"))
                    .unwrap_err()
                    .message
                    .contains("is you")
            );
            let error = desk.ask(message_req("sibling", "hi")).unwrap_err();
            assert!(
                error
                    .message
                    .contains("rests; only 'parent' can message it now")
            );
            assert!(sibling.mailbox.begin());
            assert!(desk.ask(message_req("sibling", "hi")).is_ok());
            assert!(
                matches!(next_input(&sibling.mailbox), Input::Message(message) if message.text == "hi" && !message.from_parent)
            );
        },
    );
}

#[test]
fn dismiss_joins_a_resting_helper_and_reuses_its_name() {
    with_spawnable_desk(3, |desk, fleet, agent| {
        desk.call
            .handle
            .0
            .provider
            .swap(Arc::new(Provider::scripted(
                "test-model",
                Script::new()
                    .then(Reply::tool_calls(vec![ral_call(
                        "first",
                        "exarch-agents `reply 'one'",
                    )]))
                    .then(Reply::tool_calls(vec![ral_call(
                        "second",
                        "exarch-agents `reply 'two'",
                    )])),
            )));
        with_parked(&desk, |desk, id| desk.ask(start_req(id, "go", "helper"))).unwrap();
        assert!(matches!(
            next_input(&agent.mailbox),
            Input::Child {
                status: ChildStatus::Replied(_),
                ..
            }
        ));
        let old = fleet.resolve("helper").unwrap();
        wait_at_rest(&old);
        desk.ask(Request::Agents(Agents::Dismiss("helper".into())))
            .unwrap();
        assert!(!old.is_live());
        assert!(desk.loan.get().unwrap().state.nursery.is_joined(old.id()));
        assert!(
            fleet.resolve("helper").is_none(),
            "dismiss returns only after the name is released"
        );
        with_parked(&desk, |desk, id| desk.ask(start_req(id, "again", "helper"))).unwrap();
        let newest = fleet.resolve("helper").unwrap();
        assert_ne!(old.id(), newest.id());
        assert!(matches!(next_input(&agent.mailbox), Input::Gone(id) if id == old.id()));
        assert!(matches!(
            next_input(&agent.mailbox),
            Input::Child {
                status: ChildStatus::Replied(_),
                ..
            }
        ));
        wait_at_rest(&newest);
        assert_eq!(
            str_field(&desk.agent_read("helper".into()).unwrap(), "reply"),
            Some("two")
        );
    });
}

#[test]
fn dismiss_refuses_a_grandchild_and_leaves_both_lives_untouched() {
    with_spawnable_desk(3, |desk, fleet, root| {
        let child = test_agent(
            &fleet,
            TestAgentSpec::child("helper", &root, root.mailbox.life().child()),
        )
        .unwrap();
        adopt_synthetic(&desk, &child);
        let grandchild = test_agent(
            &fleet,
            TestAgentSpec::child("grandchild", &child, child.mailbox.life().child()),
        )
        .unwrap();
        let error = desk
            .ask(Request::Agents(Agents::Dismiss("grandchild".into())))
            .unwrap_err();
        assert_eq!(
            error.message,
            "dismiss only an agent you started; its parent may dismiss it"
        );
        assert!(child.is_live());
        assert!(grandchild.is_live());
        assert!(!desk.loan.get().unwrap().state.nursery.is_joined(child.id()));
    });
}

/// The read-after-write law once more, through a real spawn: a surfaced
/// value must render before an enquiry raised after it in the same run.
#[test]
fn surface_then_spawn_observes_the_surface_first() {
    let mut session = crate::agent::Agent::for_test("system").unwrap();
    let (tx, rx) = channel();
    let emit = Emitter::new(tx, session.id());
    let _ = session.ral(
        &ral_core::process::DurableRoot::default().child(),
        r#"exarch-pins `clear "test-marker"; exarch-agents `start [prompt: #'go'#, name: 't', type: `amnemon, grant: `confined, model: `inherit]"#,
        5,
        &emit,
    );
    drop(emit);

    let mut saw_unpin = false;
    let mut saw_spawn = false;
    while let Ok(sig) = rx.try_recv() {
        match sig {
            Signal::Transient(_, Transient::Unpin { .. }) => saw_unpin = true,
            Signal::Commit(_, commit) => {
                for fact in commit.records() {
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
            }
            Signal::Born(..) | Signal::Transient(..) => {}
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
    with_desk(|mut d, _agent| {
        d.loan
            .get()
            .unwrap()
            .log
            .record_emitter()
            .attach(Box::new(FleetSink {
                id: AgentId::new(0),
                tx: tx.downgrade(),
                meter: crate::bus::UsageMeter::default(),
            }));
        d.call.emit = Emitter::new(tx, AgentId::new(0));

        for text in ["first", "second"] {
            d.ask(reply_req(FOValue::String { value: text.into() }))
                .expect("a returning agent's reply must succeed");
        }

        assert_eq!(
            d.loan.get().unwrap().state.reply.take(),
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
    });
}
