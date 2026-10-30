use super::*;
use crate::bus::{Admission, AgentMailbox, Command, Input, Next, Rest, Rewrite};
use crate::record::Commit;
use crate::schedule::{ScheduleRegistry, Trigger};
use genai::chat::Binary;
use ral_core::process::DurableRoot;
use std::time::Duration;

fn cell(log: &AgentLog, folds: (Mail, Schedules, Model)) -> AgentMailbox {
    let life = DurableRoot::default().child();
    let journal = log.record_emitter();
    journal.bind(life.clone());
    AgentMailbox::new(log.id(), life, journal, Admission::Free, None, folds)
}

fn live_root(tag: &str, returns: bool, selected: &Selected) -> (TestDir, AgentLog, AgentMailbox) {
    let sessions = sessions_root(tag);
    let (log, mail, schedules, model) = AgentLog::root(
        &sessions,
        AgentId::new(0),
        "main",
        returns,
        selected,
        "sys",
        None,
    )
    .unwrap();
    let mailbox = cell(&log, (mail, schedules, model));
    (sessions, log, mailbox)
}

fn commits(log: &AgentLog) -> Vec<Commit> {
    log.record_emitter()
        .reader()
        .since(None)
        .unwrap()
        .1
        .collect::<std::io::Result<_>>()
        .unwrap()
}

fn snapshot(mailbox: &AgentMailbox) -> String {
    mailbox
        .transact(|txn| format!("{:?}", (txn.mail(), txn.schedules(), txn.model())))
        .unwrap()
}

#[test]
fn a_failed_commit_to_preserves_the_callers_cell_and_queue() {
    for bytes in [0, 17] {
        let (_sessions, log, mailbox) =
            live_root("failed-cell-append", false, &Selected::for_test("m"));
        mailbox.address().steer("retained human".into()).unwrap();
        mailbox
            .address()
            .post(Input::Surface {
                generation: 1,
                values: vec![],
            })
            .unwrap();
        let mut timers = ScheduleRegistry::new();
        mailbox
            .transact(|txn| {
                timers
                    .schedule(
                        txn,
                        Trigger::After(Duration::from_hours(1)),
                        "wake".into(),
                        "later".into(),
                    )
                    .unwrap();
                assert!(txn.narrow("m", 100_000).unwrap());
            })
            .unwrap();
        assert!(mailbox.begin());
        let before = snapshot(&mailbox);
        let queued = mailbox.address().queued_posts();
        let prefix = fs::read(record_path(&log)).unwrap();
        log.record_emitter().fail_after(bytes);

        let error = mailbox
            .transact(|txn| {
                let result = txn.clear();
                assert!(!txn.is_live());
                result
            })
            .unwrap()
            .unwrap_err();
        assert!(
            error.to_string().contains("injected record write failure"),
            "{error}"
        );
        assert!(!mailbox.address().is_live());
        assert_eq!(snapshot(&mailbox), before);
        assert_eq!(mailbox.address().queued_posts(), queued);
        assert!(
            mailbox
                .transact(|txn| txn.change_model(Selected::for_test("other")))
                .unwrap()
                .is_err()
        );
        assert_eq!(snapshot(&mailbox), before);
        let failed = fs::read(record_path(&log)).unwrap();
        assert_eq!(&failed[..prefix.len()], prefix);
        assert_eq!(failed.len(), prefix.len() + bytes);
    }
}

#[test]
fn reopen_refuses_foreign_control_before_quarantining_a_torn_tail() {
    let (sessions, mut log, mailbox) =
        live_root("foreign-control-tail", false, &Selected::for_test("m"));
    complete_round(&mut log, "valid transcript", "valid answer");
    let path = record_path(&log);
    drop((mailbox, log));
    let foreign = Record::Control(Control::WindowNarrowed {
        model: "not-in-force".into(),
        context_window: 1,
    });
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&crate::record::commit_line(&[foreign]))
        .unwrap();
    file.write_all(b"\n{\"commit\":").unwrap();
    file.flush().unwrap();
    drop(file);
    let before = fs::read(&path).unwrap();

    let error = AgentLog::reopen(
        &sessions,
        AgentId::new(0),
        &Selected::for_test("m"),
        false,
        "sys",
        1,
    )
    .err()
    .expect("foreign control is not a recoverable tail");
    let text = error.to_string();
    assert!(
        text.contains("foreign record")
            && text.contains("WindowNarrowed")
            && text.contains("not-in-force"),
        "{text}"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(!path.with_extension("jsonl.crash").exists());
}

#[test]
fn reopen_reconciles_one_line_and_its_cell_equals_the_reopened_file() {
    let selected = Selected::for_test("m");
    let (sessions, mut log, mailbox) = live_root("reconcile-cell", true, &selected);
    complete_round(&mut log, "transcript", "answer");
    assert!(mailbox.begin());
    let job = mailbox.address().status().unwrap().job.unwrap();
    let long = "é".repeat(90);
    mailbox
        .address()
        .steer(format!("{long}\nnot the first line"))
        .unwrap();
    mailbox
        .address()
        .command(Command::Rewrite(Rewrite::Clear))
        .unwrap();
    let pending = mailbox.address().queued_posts();
    let mut timers = ScheduleRegistry::new();
    mailbox
        .transact(|txn| {
            for label in ["z", "a"] {
                timers
                    .schedule(
                        txn,
                        Trigger::After(Duration::from_hours(1)),
                        "wake".into(),
                        label.into(),
                    )
                    .unwrap();
            }
            assert!(txn.narrow("m", 100_000).unwrap());
            assert!(txn.mail().owes());
        })
        .unwrap();
    let rows = log.context().transcript_index();
    let before = commits(&log).len();
    drop((timers, mailbox, log));

    let (log, mail, schedules, model) =
        AgentLog::reopen(&sessions, AgentId::new(0), &selected, true, "new sys", 42).unwrap();
    let mailbox = cell(&log, (mail, schedules, model));
    assert_eq!(mailbox.address().queued(), Vec::<String>::new());
    mailbox
        .transact(|txn| {
            assert_eq!(txn.mail().pending().count(), 0);
            assert_eq!(txn.mail().job(), None);
            assert!(!txn.mail().owes());
            assert!(txn.schedules().is_empty());
            assert_eq!(txn.model().window(), Some(100_000));
        })
        .unwrap();
    assert_eq!(log.context().transcript_index(), rows);
    let lines = commits(&log);
    assert_eq!(
        lines.len(),
        before + 2,
        "one resume, then the changed system prompt"
    );
    let line: Vec<_> = lines[before]
        .records()
        .iter()
        .map(Recorded::value)
        .collect();
    assert!(matches!(line.as_slice(), [
        Record::Control(Control::SessionResumed { selected: choice, returns: true, at_unix_ms: 42 }),
        Record::Control(Control::Withdrawn { posts }),
        Record::Control(Control::JobSettled { job: settled, stopped: false }),
        Record::Control(Control::Unscheduled { label: a }),
        Record::Control(Control::Unscheduled { label: z }),
        Record::Forensic(Forensic::SystemNote { text }),
    ] if choice == &selected && posts.iter().copied().collect::<Vec<_>>() == pending
        && settled == &job && a == "a" && z == "z"
        && text == &format!("the restart withdrew 2 queued input(s) no one had read: human: {}; command: /clear", "é".repeat(80))));
    assert!(matches!(lines[before + 1].records().first.value(),
        Record::Transcript(Transcript::System { text }) if text == "new sys"));

    let (again, mail, schedules, model) =
        AgentLog::reopen(&sessions, AgentId::new(0), &selected, true, "new sys", 43).unwrap();
    mailbox
        .transact(|txn| {
            assert_eq!(
                (txn.mail(), txn.schedules(), txn.model()),
                (&mail, &schedules, &model)
            );
        })
        .unwrap();
    assert_eq!(
        again.context().transcript_index(),
        log.context().transcript_index()
    );
    assert_eq!(
        serde_json::to_value(again.context().rendered()).unwrap(),
        serde_json::to_value(log.context().rendered()).unwrap()
    );
    assert_eq!(
        commits(&again).len(),
        lines.len() + 1,
        "an already reconciled reopen adds only its resume"
    );
}

#[test]
fn reopen_keeps_taken_debt_but_withdraws_unread_debt() {
    for taken in [false, true] {
        let selected = Selected::for_test("m");
        let (sessions, log, mailbox) = live_root("returning-debt", true, &selected);
        mailbox.address().steer("work".into()).unwrap();
        if taken {
            let Next::Take {
                input: Input::Human(text),
                exchange,
            } = mailbox.next(Rest { pending: false })
            else {
                panic!("the owner takes the human post");
            };
            assert_eq!(text, "work");
            drop(exchange);
        }
        assert!(mailbox.address().status().unwrap().owes);
        drop((mailbox, log));
        let (log, mail, schedules, model) =
            AgentLog::reopen(&sessions, AgentId::new(0), &selected, true, "sys", 1).unwrap();
        let mailbox = cell(&log, (mail, schedules, model));
        assert_eq!(mailbox.address().status().unwrap().owes, taken);
        assert_eq!(
            mailbox.address().queued_posts(),
            Vec::<crate::record::PostId>::new()
        );
        assert_eq!(
            records(&log)
                .iter()
                .any(|record| matches!(record, Record::Control(Control::Withdrawn { .. }))),
            !taken
        );
    }
}

#[test]
fn reopen_preserves_the_frontends_narrowed_window_until_a_switch() {
    let selected = Selected {
        context_window: Some(200_000),
        ..Selected::for_test("m")
    };
    let (sessions, log, mailbox) = live_root("reopen-window", false, &selected);
    assert!(
        mailbox
            .transact(|txn| txn.narrow("m", 100_000))
            .unwrap()
            .unwrap()
    );
    drop((mailbox, log));
    for (name, listed, expected) in [
        ("m", 300_000, 100_000),
        ("m", 80_000, 80_000),
        ("n", 400_000, 400_000),
    ] {
        let selected = Selected {
            context_window: Some(listed),
            ..Selected::for_test(name)
        };
        let (log, mail, schedules, model) =
            AgentLog::reopen(&sessions, AgentId::new(0), &selected, false, "sys", 1).unwrap();
        let mailbox = cell(&log, (mail, schedules, model));
        mailbox
            .transact(|txn| {
                assert_eq!(txn.model().selected(), &selected);
                assert_eq!(txn.model().window(), Some(expected));
            })
            .unwrap();
        let blocks = crate::record::replay::<crate::record::View>(
            log.dir().open(RECORD).unwrap(),
            &record_path(&log),
            crate::record::Blocks::default(),
        )
        .unwrap();
        assert_eq!(blocks.context_window(), Some(expected));
        if name == "n" {
            assert!(
                mailbox
                    .transact(|txn| txn.narrow("n", 50_000))
                    .unwrap()
                    .unwrap()
            );
            mailbox
                .transact(|txn| txn.change_model(selected.clone()))
                .unwrap()
                .unwrap();
            assert_eq!(
                mailbox.transact(|txn| txn.model().window()).unwrap(),
                Some(listed)
            );
        }
    }
}

#[test]
fn a_fork_preserves_a_noted_eviction_beside_an_empty_inherited_spawning_turn() {
    let mut parent = fresh_root();
    complete_round(&mut parent, "earlier", "done");
    parent
        .append_user("spawn".into(), None, Voice::Human)
        .unwrap();
    parent
        .append_assistant(assistant_with_tool("spawn"), vec!["spawn".into()], None)
        .unwrap();
    parent
        .evict(
            span(1, 3),
            Some("keep the parent's note".into()),
            EditAuthority::Model,
        )
        .unwrap();
    let original = parent.inherited_context().turns.pop().unwrap().at;
    let mut child = mnemon(&parent, AgentId::new(1));
    let inherited = child.context().context_survey().rows;
    assert_eq!(
        (inherited[1].span, inherited[1].bytes),
        (Span::single(4), 0)
    );
    assert_eq!(
        child.context().rendered().len(),
        1,
        "read_seeds cut the unanswered spawning call to an empty prefix"
    );
    child
        .append_user("launch".repeat(1000), None, Voice::Human)
        .unwrap();
    let launch_bytes = child.context().context_survey().rows.last().unwrap().bytes;
    for keep in [0, 1, launch_bytes - 1] {
        assert_eq!(
            child.context().plan_forced_eviction(keep),
            None,
            "keep {keep}"
        );
        assert!(
            head_eviction(&child)
                .unwrap()
                .contains("keep the parent's note")
        );
        let linked = child
            .inherited_context()
            .turns
            .into_iter()
            .find(|turn| turn.row.id == 4)
            .unwrap()
            .at;
        assert_eq!(linked.source, original.source);
        assert_eq!(linked.loci, original.loci);
        let read = child.context().read_transcript(&[4]).unwrap();
        assert!(
            read.iter()
                .flat_map(|turn| &turn.messages)
                .flat_map(|message| &message.parts)
                .any(
                    |part| matches!(part, TranscriptPart::Program { source, .. } if source == "pwd")
                )
        );
    }
}

#[test]
fn forced_eviction_still_plans_ordinary_material_and_two_evictions() {
    let mut ordinary = fresh_root();
    complete_round(&mut ordinary, "ordinary", "answer");
    ordinary
        .append_user("launch".repeat(1000), None, Voice::Human)
        .unwrap();
    assert_eq!(ordinary.context().plan_forced_eviction(0), Some(span(1, 2)));

    let mut two = fresh_root();
    for text in ["one", "two", "three"] {
        complete_round(&mut two, text, text);
    }
    two.evict(span(1, 2), Some("first".into()), EditAuthority::Model)
        .unwrap();
    two.evict(span(3, 6), Some("second".into()), EditAuthority::Model)
        .unwrap();
    two.append_user("launch".repeat(1000), None, Voice::Human)
        .unwrap();
    assert_eq!(two.context().plan_forced_eviction(0), Some(span(1, 6)));
}

#[test]
fn imported_binary_reads_are_total_metadata_and_grep_never_reads_payloads() {
    let (sessions, mut log, mailbox) =
        live_root("binary-metadata", false, &Selected::for_test("m"));
    let padding = "=".repeat(4096);
    let fixtures = [
        ("", 0),
        ("Zg==", 1),
        ("Zm8=", 2),
        ("Zm9v", 3),
        ("Zm9vYg==", 4),
        ("=", 0),
        ("==", 0),
        ("====", 0),
        (padding.as_str(), 0),
    ];
    let mut parts = vec![ContentPart::Text(
        "needle next to malformed attachment".into(),
    )];
    parts.extend(fixtures.iter().enumerate().map(|(index, (payload, _))| {
        ContentPart::Binary(Binary::from_base64(
            "application/octet-stream",
            *payload,
            Some(format!("part-{index}")),
        ))
    }));
    parts.push(ContentPart::Binary(Binary::from_url(
        "image/png",
        "https://example.invalid/payload",
        Some("remote.png".into()),
    )));
    log.import_note(ChatMessage::user(parts)).unwrap();
    complete_round(&mut log, "next", "done");
    let check = |log: &AgentLog| {
        let read = log.context().read_transcript(&[1]).unwrap();
        let parts = &read[0].messages[0].parts;
        assert_eq!(parts.len(), fixtures.len() + 2);
        for (index, (_, expected)) in fixtures.iter().enumerate() {
            assert!(
                matches!(&parts[index + 1], TranscriptPart::Binary { content_type, name, bytes }
                if content_type == "application/octet-stream" && name == &format!("part-{index}") && bytes == expected)
            );
        }
        assert!(
            matches!(parts.last(), Some(TranscriptPart::Binary { content_type, name, bytes: 0 })
            if content_type == "image/png" && name == "remote.png")
        );
        let answer = log
            .context()
            .grep_transcript(&regex("needle"), Some(&[1]))
            .unwrap();
        assert_eq!(answer.total, 1);
        assert_eq!(answer.hits[0].text, "needle next to malformed attachment");
        assert_eq!(
            log.context()
                .grep_transcript(&regex("=|Zg|Zm|https://|part-"), Some(&[1]))
                .unwrap()
                .total,
            0
        );
    };
    check(&log);
    log.evict(Span::single(1), None, EditAuthority::Harness)
        .unwrap();
    check(&log);
    drop((mailbox, log));
    let (log, ..) = AgentLog::reopen(
        &sessions,
        AgentId::new(0),
        &Selected::for_test("m"),
        false,
        "sys",
        1,
    )
    .unwrap();
    check(&log);
}

#[test]
fn a_recorded_eviction_reopens_rewinds_and_forks_with_the_same_index() {
    let selected = Selected::for_test("m");
    let (sessions, mut log, mailbox) = live_root("eviction-lineage", false, &selected);
    for text in ["one", "two", "three"] {
        complete_round(&mut log, text, text);
    }
    let original = format!(
        "{:?}",
        log.context().read_transcript(&[1, 2, 3, 4, 5, 6]).unwrap()
    );
    log.evict(
        span(1, 2),
        Some("first answer".into()),
        EditAuthority::Model,
    )
    .unwrap();
    assert!(records(&log).iter().any(|record| matches!(record,
        Record::Transcript(Transcript::Evicted(eviction))
            if eviction.span == span(1, 2) && eviction.note.as_deref() == Some("first answer"))));
    let rows = log.context().transcript_index();
    drop((mailbox, log));
    let (mut log, ..) =
        AgentLog::reopen(&sessions, AgentId::new(0), &selected, false, "sys", 1).unwrap();
    assert_eq!(log.context().transcript_index(), rows);
    log.rewind(3).unwrap();
    let rows = log.context().transcript_index();
    assert_eq!(
        rows.iter()
            .map(|row| (row.span, row.location))
            .collect::<Vec<_>>(),
        [
            (span(1, 2), Location::Resident),
            (Span::single(1), under(1, 2)),
            (Span::single(2), under(1, 2)),
            (Span::single(3), Location::Withdrawn),
            (Span::single(4), Location::Withdrawn),
            (Span::single(5), Location::Withdrawn),
            (Span::single(6), Location::Withdrawn),
        ]
    );
    drop(log);
    let (log, ..) =
        AgentLog::reopen(&sessions, AgentId::new(0), &selected, false, "sys", 2).unwrap();
    assert_eq!(log.context().transcript_index(), rows);
    let child = mnemon(&log, AgentId::new(1));
    let inherited: Vec<_> = rows
        .into_iter()
        .map(|mut row| {
            if row.kind != ItemKind::Eviction {
                row.kind = ItemKind::Turn(TurnKind::Inherited);
            }
            row
        })
        .collect();
    assert_eq!(child.context().transcript_index(), inherited);
    assert_eq!(child.context().evictions(), log.context().evictions());
    assert_eq!(
        format!(
            "{:?}",
            child
                .context()
                .read_transcript(&[1, 2, 3, 4, 5, 6])
                .unwrap()
        ),
        original
    );
}
