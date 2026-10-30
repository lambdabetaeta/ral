use super::*;

#[test]
fn resumed_agent_adds_only_the_fresh_shell_note() {
    let dir = tmp("resume-agent");
    let sessions = dir.make_dir("sessions").unwrap();
    let (mut log, _, _, _) = AgentLog::root(
        &sessions,
        AgentId::new(0),
        "main",
        false,
        &Selected::for_test("old-model"),
        "system",
        None,
    )
    .unwrap();
    log.append_user("before the crash".into(), None, Voice::Human)
        .unwrap();
    log.append_assistant(ChatMessage::assistant("saved answer"), vec![], None)
        .unwrap();
    let before = log.context().rendered();
    drop(log);
    let [(agent, resumed)] = <[_; 1]>::try_from(
        Agent::resume(
            root_config(&dir, 0),
            identity_engine("resume-agent"),
            &scripted("new-model", Script::new()),
        )
        .unwrap(),
    )
    .unwrap_or_else(|roots| panic!("one open session, got {}", roots.len()));
    assert_eq!(resumed.turn, 2);
    assert!(agent.is_ready());
    let messages = agent.rendered_messages();
    assert_eq!(
        serde_json::to_vec(&messages[..before.len()]).unwrap(),
        serde_json::to_vec(&before).unwrap()
    );
    assert_eq!(messages.len(), before.len() + 1);
    let note = messages.last().unwrap();
    assert_eq!(note.role, genai::chat::ChatRole::User);
    let text = note.content.first_text().unwrap();
    for loss in [
        "shell is fresh",
        "bindings",
        "workers",
        "cwd",
        "scratch",
        "pinned state",
        "scheduled events",
        "sub-agents",
    ] {
        assert!(text.contains(loss), "resume note must name {loss}: {text}");
    }
    let records = crate::record::read_records(&dir.path().join("sessions/0/record.jsonl")).unwrap();
    assert!(records.iter().any(|record| matches!(
        record,
        crate::record::Record::Control(crate::record::Control::SessionResumed { .. })
    )));
}

#[test]
fn resume_seeds_child_ids_past_existing_session_directories() {
    let dir = tmp("resume-id-seed");
    let sessions = dir.make_dir("sessions").unwrap();
    let (log, _, _, _) = AgentLog::root(
        &sessions,
        AgentId::new(0),
        "main",
        false,
        &Selected::for_test("old-model"),
        "system",
        None,
    )
    .unwrap();
    let sentinel = sessions.path().join("1/sentinel");
    fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
    fs::write(&sentinel, b"keep").unwrap();
    drop(log);
    let (root, _) = Agent::resume(
        root_config(&dir, 1),
        identity_engine("resume-id-seed"),
        &scripted("new-model", Script::new()),
    )
    .unwrap()
    .remove(0);
    let child = root.fork().unwrap();
    assert!(child.id().get() >= 2);
    assert_eq!(fs::read(&sentinel).unwrap(), b"keep");
}

#[test]
fn resumed_roots_keep_their_recorded_report_without_reporting_again() {
    use crate::record::{ChildStatus, Record, read_records};
    use ral_core::data::FOValue;
    for trunk in [Trunk::Returns, Trunk::Converses] {
        let dir = tmp("resume-published-status");
        let sessions = dir.make_dir("sessions").unwrap();
        let (log, mail, schedules, model) = AgentLog::root(
            &sessions,
            AgentId::new(0),
            "main",
            false,
            &Selected::for_test("old-model"),
            "system",
            None,
        )
        .unwrap();
        let status = ChildStatus::Replied(FOValue::Int { value: 1 });
        let life = ral_core::process::DurableRoot::default().child();
        let mailbox = crate::bus::AgentMailbox::new(
            log.id(),
            life,
            log.record_emitter(),
            Admission::Free,
            None,
            (mail, schedules, model),
        );
        mailbox
            .try_transact(|txn| {
                txn.report(status.clone(), |_| {
                    crate::bus::Input::Human("unused".into())
                })
            })
            .unwrap();
        drop(mailbox);
        let record = log.dir().path().join(RECORD);
        drop(log);
        let (root, _) = Agent::resume(
            RootConfig {
                trunk,
                ..root_config(&dir, 0)
            },
            identity_engine("resume-published-status"),
            &scripted("new-model", Script::new()),
        )
        .unwrap()
        .remove(0);
        assert_eq!(root.handle().status().unwrap().report, Some(status.clone()));
        assert!(!root.handle().status().unwrap().owes);
        drop(root);
        let statuses = read_records(&record)
            .unwrap()
            .into_iter()
            .filter_map(|record| match record {
                Record::Control(crate::record::Control::Status { status }) => Some(status),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(statuses, vec![status]);
    }
}
