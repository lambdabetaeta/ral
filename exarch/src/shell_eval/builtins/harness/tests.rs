use crate::agent::Agent;
use crate::agent::testkit::{
    TestAgentSpec, close_exchange, pop, ral_call, scripted, set_provider, test_agent,
    wait_for_settle,
};
use crate::bus::{AgentOutcome, AgentResult, Emitter, Item, Next, channel};
use crate::provider::scripted::{Reply, Script};
use crate::record::Voice;
use ral_core::process::DurableRoot;

fn run(session: &Agent, source: &str, budget: u64) -> String {
    let (tx, _rx) = channel();
    session
        .ral(
            &DurableRoot::default().child(),
            source,
            budget,
            &Emitter::new(tx, session.id()),
        )
        .text
}

fn eval(session: &Agent, source: &str) -> String {
    run(session, source, 5)
}

fn trunk() -> Agent {
    Agent::for_test("system").unwrap()
}

fn schedules(session: &Agent) -> crate::schedule::ScheduleRegistry {
    session
        .host_services(&crate::bus::dummy_emitter().0)
        .schedules
}

fn no_children(session: &Agent) {
    assert_eq!(
        crate::agent::roster::summary(&session.handle(), &session.fleet).live,
        0,
    );
}

fn bare_grant_tags() -> impl Iterator<Item = &'static str> {
    std::iter::once("inherit").chain(crate::policy::SPAWN_BASES)
}

#[test]
fn unknown_grant_label_errors_before_any_enquiry_crosses() {
    let session = trunk();
    let result = eval(
        &session,
        r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `bogus, model: `inherit]",
    );
    for label in bare_grant_tags() {
        assert!(result.contains(label), "must name `{label}`, got: {result}");
    }
    no_children(&session);
}

#[test]
fn dangerous_is_no_longer_a_spawn_grant_and_the_refusal_points_at_inherit() {
    let session = trunk();
    let result = eval(
        &session,
        r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `dangerous, model: `inherit]",
    );
    assert!(
        result.contains("inherit is how you decline to narrow"),
        "{result}"
    );
    no_children(&session);
}

#[test]
fn a_bare_restrict_errors_before_any_enquiry_crosses() {
    let session = trunk();
    let result = eval(
        &session,
        r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `restrict, model: `inherit]",
    );
    assert!(result.contains("capability record"), "{result}");
    no_children(&session);
}

#[test]
fn inherit_and_restrict_pass_the_grant_door() {
    let session = trunk();
    for grant in ["`inherit", "`restrict [net: `offline]"] {
        let result = eval(
            &session,
            &format!(
                r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: {grant}, model: `guess]"
            ),
        );
        assert!(
            result.contains("`model") && !result.contains("`grant"),
            "{grant}: {result}"
        );
        no_children(&session);
    }
}

#[test]
fn unknown_type_tag_errors_before_any_enquiry_crosses() {
    let session = trunk();
    let result = eval(
        &session,
        r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `bogus, grant: `confined, model: `inherit]",
    );
    assert!(result.contains("amnemon"), "{result}");
    assert!(result.contains("mnemon"), "{result}");
    no_children(&session);
}

#[test]
fn unknown_selection_tag_errors_naming_both_arms() {
    let session = trunk();
    let result = eval(
        &session,
        r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `confined, model: `guess]",
    );
    for arm in ["inherit", "role"] {
        assert!(result.contains(arm), "must name `{arm}, got: {result}");
    }
    no_children(&session);
}

#[test]
fn an_empty_role_name_is_refused() {
    let session = trunk();
    let result = eval(
        &session,
        r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `confined, model: `role '']",
    );
    assert!(result.contains("non-empty"), "{result}");
    no_children(&session);
}

#[test]
fn invalid_name_errors_before_any_enquiry_crosses() {
    let session = trunk();
    let result = eval(
        &session,
        r#"exarch-agents `start [prompt: #'hi'#, name: "has space", type: `amnemon, grant: `confined, model: `inherit]"#,
    );
    assert!(result.contains("name"), "{result}");
    no_children(&session);
}

#[test]
fn unknown_outer_tag_reaches_the_door_naming_every_legal_label() {
    let result = eval(&trunk(), "exarch-agents `stop 'x'");
    for tag in ["list", "start", "message", "cancel", "reply", "read"] {
        assert!(result.contains(tag), "must name `{tag}, got: {result}");
    }
}

#[test]
fn missing_agent_field_errors_statically_naming_the_field() {
    let session = trunk();
    let result = eval(
        &session,
        r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, model: `inherit]",
    );
    assert!(result.contains("field named 'grant'"), "{result}");
    no_children(&session);
}

#[test]
fn misspelled_start_field_errors_statically_naming_the_field() {
    let session = trunk();
    let result = eval(
        &session,
        r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grnat: `confined, model: `inherit]",
    );
    assert!(result.contains("no field named 'grnat'"), "{result}");
    no_children(&session);
}

fn start_child(session: &Agent, name: &str, prompt: &str, command: &str) -> AgentResult {
    set_provider(
        session,
        scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call("reply-1", command)])),
        ),
    );
    let result = eval(
        session,
        &format!(
            "exarch-agents `start [prompt: #'{prompt}'#, name: '{name}', type: `amnemon, grant: `read-only, model: `inherit]"
        ),
    );
    assert!(
        result.contains("live: 1"),
        "the summary must count the child: {result}"
    );
    match wait_for_settle(crate::agent::testkit::inbox(session)) {
        Next::Item(Item::Agent(result)) => result,
        other => panic!("expected an Agent result item, got: {other:?}"),
    }
}

#[test]
fn agent_full_stack_round_trip_answers_the_summary_and_parks_a_reply() {
    let session = trunk();
    let result = start_child(
        &session,
        "helper",
        "say hi",
        "exarch-agents `reply 'say hi'",
    );
    let notice = result.outcome.marked_item(&result.name, result.elapsed);
    assert!(notice.contains("exarch-agents `read 'helper'"), "{notice}");
    let read = eval(&session, "exarch-agents `read 'helper'");
    assert!(read.contains("say hi"), "{read}");
    let roster = eval(&session, "exarch-agents `list");
    assert!(roster.contains("replied"), "{roster}");
}

#[test]
fn agents_cancel_answer_excludes_the_ended_agent_before_teardown() {
    let session = trunk();
    let mailbox = session
        .host_services(&crate::bus::dummy_emitter().0)
        .mailbox;
    let doomed = TestAgentSpec::child("doomed", &session.handle(), mailbox);
    let _doomed = test_agent(&session.fleet, doomed).expect("a fresh child of a live parent");
    let result = eval(&session, "exarch-agents `cancel 'doomed'");
    assert!(result.contains("EXIT: 0"), "{result}");
    assert!(
        result.contains("live: 0"),
        "cancelled life is no longer live: {result}"
    );
}

#[test]
fn bad_cron_expr_errors_before_any_enquiry_crosses() {
    let session = trunk();
    let result = eval(
        &session,
        "exarch-schedules `add [trigger: `cron '* * * *', label: 'nightly', prompt: #'wake'#]",
    );
    assert!(result.contains("five fields"), "{result}");
    assert!(schedules(&session).list().is_empty());
}

#[test]
fn bad_duration_errors_before_any_enquiry_crosses() {
    let session = trunk();
    let result = eval(
        &session,
        "exarch-schedules `add [trigger: `after 'nope', label: 'nightly', prompt: #'wake'#]",
    );
    assert!(result.contains("duration"), "{result}");
    assert!(schedules(&session).list().is_empty());
}

#[test]
fn trigger_neither_cron_nor_after_errors_before_any_enquiry_crosses() {
    let session = trunk();
    let result = eval(
        &session,
        "exarch-schedules `add [trigger: `bogus 'x', label: 'nightly', prompt: #'wake'#]",
    );
    assert!(result.contains("cron"), "{result}");
    assert!(result.contains("after"), "{result}");
    assert!(schedules(&session).list().is_empty());
}

#[test]
fn missing_spec_field_errors_statically_naming_the_field() {
    let session = trunk();
    let result = eval(
        &session,
        "exarch-schedules `add [trigger: `after '1s', label: 'nightly']",
    );
    assert!(
        result.contains("missing a field named 'prompt'"),
        "{result}"
    );
    assert!(schedules(&session).list().is_empty());
}

#[test]
fn unknown_extra_spec_field_errors_statically_naming_the_field() {
    let session = trunk();
    let result = eval(
        &session,
        "exarch-schedules `add [trigger: `after '1s', label: 'nightly', prompt: #'wake'#, extra: 1]",
    );
    assert!(result.contains("no field named 'extra'"), "{result}");
    assert!(schedules(&session).list().is_empty());
}

fn granted_trunk() -> Agent {
    Agent::for_test_with(crate::agent::TestTrunk {
        allow_schedule: true,
        ..crate::agent::TestTrunk::new("system")
    })
    .expect("a granted test trunk")
}

const NIGHTLY: &str =
    "exarch-schedules `add [trigger: `after '10m', label: 'nightly', prompt: #'wake'#]";

#[test]
fn schedule_add_answer_carries_the_new_row() {
    let result = eval(&granted_trunk(), NIGHTLY);
    assert!(result.contains("nightly"), "{result}");
}

#[test]
fn schedule_full_stack_round_trip_answers_the_table_and_fires_into_inbox() {
    let session = granted_trunk();
    let result = eval(
        &session,
        "exarch-schedules `add [trigger: `after '1s', label: 'nightly', prompt: #'wake'#]",
    );
    assert!(result.contains("EXIT: 0"), "{result}");
    assert!(result.contains("next-s"), "{result}");
    let live = schedules(&session).list();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].label, "nightly");
    assert!(result.contains("nightly"), "{result}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match pop(crate::agent::testkit::inbox(&session)) {
            Some(Next::Item(Item::Wakeup(text))) => {
                assert!(text.contains("wake"), "{text}");
                break;
            }
            Some(other) => panic!("expected a Wakeup item, got: {other:?}"),
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

#[test]
fn schedule_remove_full_stack_disarms_by_label() {
    let session = granted_trunk();
    let result = eval(&session, NIGHTLY);
    assert!(result.contains("EXIT: 0"), "{result}");
    assert_eq!(schedules(&session).list().len(), 1);
    let result = eval(&session, "exarch-schedules `remove 'nightly'");
    assert!(result.contains("EXIT: 0"), "{result}");
    assert!(!result.contains("nightly"), "{result}");
    assert!(schedules(&session).list().is_empty());
    let miss = eval(&session, "exarch-schedules `remove 'nightly'");
    assert!(miss.contains("EXIT: 0"), "{miss}");
}

#[test]
fn schedule_remove_answer_omits_the_removed_row_but_keeps_the_other() {
    let session = granted_trunk();
    eval(&session, NIGHTLY);
    eval(
        &session,
        "exarch-schedules `add [trigger: `after '10m', label: 'daily', prompt: #'wake'#]",
    );
    let result = eval(&session, "exarch-schedules `remove 'nightly'");
    assert!(!result.contains("nightly"), "{result}");
    assert!(result.contains("daily"), "{result}");
}

#[test]
fn reply_full_stack_round_trip_delivers_structured_record_to_parent_inbox() {
    let session = trunk();
    start_child(
        &session,
        "finder",
        "find files",
        r#"let found = ["a.rs", "b.rs"]; exarch-agents `reply [files: $found]"#,
    );
    let read = eval(&session, "exarch-agents `read 'finder'");
    assert!(
        read.contains("files:") && read.contains("a.rs") && read.contains("b.rs"),
        "{read}"
    );
}

#[test]
fn reply_refuses_a_non_first_order_value_and_does_not_terminate() {
    let session = trunk();
    let result = eval(&session, "exarch-agents `reply { echo hi }");
    assert!(
        result.contains("T0074") && result.contains("data"),
        "{result}"
    );
    let ok = eval(&session, "exarch-agents `reply 42");
    assert!(ok.contains("EXIT: 0"), "{ok}");
}

#[test]
fn double_reply_in_one_exchange_is_last_wins() {
    let mut session = trunk();
    set_provider(
        &session,
        scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "c1",
                r#"exarch-agents `reply "first"; exarch-agents `reply "second""#,
            )])),
        ),
    );
    let (tx, _rx) = channel();
    let emit = Emitter::new(tx, session.id());
    let provider = session.handle().current_provider();
    let outcome = session.deliberate(
        &DurableRoot::default().child(),
        &provider,
        Some(("go".into(), Voice::Human)),
        None,
        &emit,
    );
    match outcome {
        Ok(crate::agent::outcome::Outcome::Replied) => {
            assert_eq!(
                session.handle().reply(),
                Some(ral_core::data::FOValue::String {
                    value: "second".into()
                }),
            );
        }
        other => panic!("expected Replied, got {other:?}"),
    }
}

#[test]
fn pin_read_full_stack_round_trip_returns_canonical_card_to_parent() {
    let session = trunk();
    let result = start_child(
        &session,
        "pinner",
        "pin and read back",
        r#"exarch-pins `set [key: "note", body: `card ["hi there"]]; exarch-agents `reply !{exarch-pins `read "note"}"#,
    );
    assert!(
        matches!(result.outcome, AgentOutcome::Replied),
        "{:?}",
        result.outcome
    );
    let read = eval(&session, "exarch-agents `read 'pinner'");
    assert!(
        read.contains("`card") && read.contains("`text [spans:"),
        "{read}"
    );
}

#[test]
fn a_reply_missing_a_projected_field_is_refused_at_the_read() {
    let session = trunk();
    start_child(
        &session,
        "lazy",
        "reply without a verdict",
        "exarch-agents `reply [note: 'all fine']",
    );
    let result = eval(
        &session,
        "let r = exarch-agents `read 'lazy'\necho $r[reply][verdict]",
    );
    assert!(
        result.contains("exarch-agents: the value at `/reply` has no field `verdict`"),
        "{result}"
    );
}

#[test]
fn pin_read_full_stack_absent_key_replies_none() {
    let session = trunk();
    let result = start_child(
        &session,
        "reader",
        "read an absent key",
        r#"exarch-agents `reply !{exarch-pins `read "nope"}"#,
    );
    assert!(
        matches!(result.outcome, AgentOutcome::Replied),
        "{:?}",
        result.outcome
    );
    let read = eval(&session, "exarch-agents `read 'reader'");
    assert!(read.contains("reply: `none"), "{read}");
}

#[test]
fn kit_round_trip_holds_every_field_including_tags_and_notes() {
    const BUDGET: u64 = 60;
    let session = trunk();
    for source in [
        r#"exarch-tasks `add "fix the parser""#,
        r#"exarch-tasks `add "write docs""#,
        "exarch-tasks `status [id: 1, status: `doing]",
        r#"exarch-tasks `tag [id: 1, tag: "urgent"]"#,
        r#"exarch-tasks `note [id: 1, note: "blocked on review"]"#,
    ] {
        run(&session, source, BUDGET);
    }
    let listed = run(&session, "exarch-tasks `list", BUDGET);
    for field in [
        "fix the parser",
        "`doing",
        "urgent",
        "blocked on review",
        "write docs",
    ] {
        assert!(listed.contains(field), "missing {field}: {listed}");
    }
    let read = run(
        &session,
        r#"let [decoded-task, _] = !{tasks-decode !{exarch-pins `read "tasks"}}
           echo $decoded-task[desc]
           echo $decoded-task[status]
           echo !{intercalate "," $decoded-task[tags]}
           echo $decoded-task[notes]"#,
        BUDGET,
    );
    for field in ["fix the parser", "doing", "urgent", "blocked on review"] {
        assert!(read.contains(field), "missing {field}: {read}");
    }
}

#[test]
fn add_task_inside_a_function_body_survives_the_block_and_the_call() {
    let session = trunk();
    eval(
        &session,
        r#"let f = { exarch-tasks `add "inside a block" }; !{f}"#,
    );
    let listed = eval(&session, "exarch-tasks `list");
    assert!(listed.contains("inside a block"), "{listed}");
}

#[test]
fn sub_agent_pinning_tasks_leaves_the_parents_register_untouched() {
    let session = trunk();
    eval(&session, r#"exarch-tasks `add "parent task""#);
    start_child(
        &session,
        "tasker",
        "add a task",
        r#"exarch-tasks `add "child task"; exarch-agents `reply "done""#,
    );
    let listed = eval(&session, "exarch-tasks `list");
    assert!(listed.contains("parent task"), "{listed}");
    assert!(!listed.contains("child task"), "{listed}");
}

#[test]
fn an_all_done_list_stays_pinned_until_cleared() {
    let session = trunk();
    eval(&session, r#"exarch-tasks `add "only task""#);
    eval(&session, "exarch-tasks `status [id: 1, status: `done]");
    let listed = eval(&session, "exarch-tasks `list");
    for field in ["only task", "`done"] {
        assert!(listed.contains(field), "missing {field}: {listed}");
    }
    eval(&session, "exarch-tasks `clear");
    let read = eval(&session, r#"exarch-pins `read "tasks""#);
    assert!(read.contains("`none") && !read.contains("`some"), "{read}");
    eval(&session, r#"exarch-tasks `add "fresh""#);
    let listed = eval(&session, "exarch-tasks `list");
    for field in ["id: 1", "fresh", "`open"] {
        assert!(listed.contains(field), "missing {field}: {listed}");
    }
}

#[test]
fn a_foreign_card_under_tasks_fails_the_kit_didactically() {
    let session = trunk();
    eval(
        &session,
        r#"exarch-pins `set [key: "tasks", body: `card [`text [spans: [[text: "not task shaped"]]]]]"#,
    );
    let result = eval(&session, r#"exarch-tasks `add "x""#);
    assert!(
        result.contains("tasks: the card under the 'tasks' pin is not task-shaped"),
        "{result}"
    );
}

fn trunk_with_an_answered_prompt() -> Agent {
    let session = trunk();
    close_exchange(&session, "first prompt", "first answer");
    session
}

#[test]
fn unknown_context_tag_reaches_the_door_naming_every_legal_tag() {
    let result = eval(&trunk(), "exarch-context `rewind [3]");
    for tag in ["survey", "evict"] {
        assert!(result.contains(tag), "must name `{tag}, got: {result}");
    }
}

#[test]
fn misspelled_evict_field_errors_statically_naming_the_field() {
    let result = eval(
        &trunk(),
        "exarch-context `evict [form: 1, to: 1, note: 'x']",
    );
    assert!(
        !result.contains("EXIT: 0") && result.contains("from"),
        "{result}"
    );
}

#[test]
fn a_missing_or_foreign_key_is_a_type_error_naming_it() {
    let session = trunk();
    for (call, key) in [
        ("exarch-context `evict [from: 1, to: 1]", "note"),
        ("exarch-context `evict [turns: [1], note: 'x']", "from"),
        ("exarch-transcript `grep [pattern: 'x']", "turns"),
        (
            "exarch-transcript `grep [pattern: 'x', turns: [1]]",
            "turns",
        ),
    ] {
        let result = eval(&session, call);
        assert!(
            !result.contains("EXIT: 0") && result.contains(key),
            "`{call}` must name `{key}`: {result}"
        );
    }
}

#[test]
fn an_eviction_answers_the_survey_it_leaves_behind() {
    let session = trunk_with_an_answered_prompt();
    close_exchange(&session, "second prompt", "second answer");
    let result = eval(
        &session,
        "exarch-context `evict [from: 1, to: 2, note: 'the old work is done']",
    );
    assert!(result.contains("EXIT: 0"), "{result}");
    assert!(result.contains("total-bytes"), "{result}");
    assert!(!result.contains("bytes-delta"), "{result}");
}

#[test]
fn transcript_answers_turn_records_with_variant_parts() {
    let session = trunk_with_an_answered_prompt();
    let result = eval(
        &session,
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
    );
    assert!(result.contains("\n2\n"), "{result}");
    assert!(result.contains("1 user"), "{result}");
    assert!(
        result.contains("role=user") && result.contains("text=first prompt"),
        "{result}"
    );
    assert!(
        result.contains("role=assistant") && result.contains("text=first answer"),
        "{result}"
    );
}

#[test]
fn transcript_read_addresses_turns_wherever_they_lie() {
    let session = trunk_with_an_answered_prompt();
    close_exchange(&session, "second prompt", "second answer");
    let result = eval(
        &session,
        r"let spanning = exarch-transcript `read [turns: [2, 3]]
           echo !{length $spanning}
           echo $spanning[0][turn] $spanning[0][role] !{length $spanning[0][messages]}
           echo $spanning[1][turn] $spanning[1][role]
           let built = exarch-transcript `read [turns: !{range 3 5}]
           echo !{length $built} $built[1][turn]",
    );
    for text in ["\n2\n", "2 assistant 1", "3 user", "2 4"] {
        assert!(result.contains(text), "missing {text:?}: {result}");
    }
}

#[test]
fn unknown_transcript_tag_reaches_the_door_naming_every_legal_tag() {
    let result = eval(&trunk(), "exarch-transcript `search 'x'");
    for tag in ["index", "read", "grep"] {
        assert!(result.contains(tag), "must name `{tag}, got: {result}");
    }
}

#[test]
fn index_and_grep_answer_their_own_shapes() {
    let session = trunk_with_an_answered_prompt();
    let result = eval(
        &session,
        r"let listed = exarch-transcript `index
           echo $listed[0][from] $listed[0][to] $listed[0][role] $listed[0][kind]
           case $listed[0][location] [
             `resident: { |_| echo location=resident },
             `under: { |_| echo location=under },
             `withdrawn: { |_| echo location=withdrawn },
           ]
           let matched = exarch-transcript `grep [pattern: 'first (prompt|answer)', turns: `all]
           echo !{length $matched[hits]} $matched[total]
           echo hits $matched[hits][0][from] $matched[hits][1][to] $matched[hits][1][kind]
           let narrowed = exarch-transcript `grep [pattern: 'first', turns: `only [2]]
           echo $narrowed[total]
           let missed = exarch-transcript `grep [pattern: 'nothing here', turns: `only [1, 2]]
           echo $missed[total]",
    );
    assert!(
        result.contains("1 1 user own") && result.contains("location=resident"),
        "{result}"
    );
    for text in ["2 2", "hits 1 2 own", "\n1\n", "\n0\n"] {
        assert!(result.contains(text), "missing {text:?}: {result}");
    }
}

#[test]
fn a_run_with_no_fork_door_refuses_in_its_own_words() {
    use ral_core::protocol::Run;
    use ral_core::shell::run::{RunReport, RunRequest};
    use ral_core::shell::runtime::Break;
    use ral_core::source::SourceOrigin;

    let mut shell = crate::boot::test_shell();
    let report = shell.run(RunRequest::from(Run::captured(
        r"exarch-agents `start [prompt: #'hi'#, name: 't', type: `amnemon, grant: `inherit, model: `inherit]",
        SourceOrigin::named("<test>"),
    )));
    let RunReport::Ran { ending, .. } = report else {
        panic!("well-formed source must run")
    };
    match ending.into_result() {
        Err(Break::Error(e)) => assert_eq!(
            e.message,
            "exarch-agents: this host adopts no forked sessions"
        ),
        other => panic!("expected the no-fork-door refusal, got {other:?}"),
    }
}
