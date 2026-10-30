use super::apply_model_switch;
use crate::agent::testkit;
use crate::host::{Host, Trunk};
use crate::port::Signal;
use crate::provider::{
    Account, Bureau, Choice, Pick, ReasoningEffort, Tuning, Via, scripted::Script, state,
};
use crate::record::Control;
use crate::record::{AgentId, Commit, Forensic, Record};
use crate::tui::app::tests::app_for_host;
use crate::tui::render::status_model;
use ral_core::own::OwnDir;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

fn host(scripts: Vec<Script>) -> Host {
    let mut root = testkit::trunk(Trunk::Converses);
    Arc::get_mut(&mut root.fleet).unwrap().launch.bureau = Arc::new(Bureau::scripted(scripts));
    testkit::host(root)
}

fn switch_commit(host: &Host, id: AgentId, mut matches: impl FnMut(&Record) -> bool) -> Commit {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Signal::Commit(agent, commit) = host
            .feed()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the model command publishes its result")
            && agent == id
            && commit
                .records()
                .iter()
                .any(|record| matches(record.value()))
        {
            return commit;
        }
    }
}

fn wait_for_pick(host: &Host, id: AgentId, pick: &Pick) {
    let deadline = Instant::now() + WAIT;
    while host.port().row(id).is_none_or(|row| {
        row.selected.model != pick.choice.model
            || row.selected.account != pick.choice.account.id.as_str()
            || row.selected.via != pick.choice.via
            || row.selected.tuning != pick.tuning
    }) {
        assert!(
            Instant::now() < deadline,
            "the agent publishes its new pick"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn status_tracks_publication_while_persistence_tracks_each_live_record_including_via() {
    let project = OwnDir::for_test("tui-landed-model").unwrap();
    let host = host(vec![Script::new(), Script::new()]);
    let mut app = app_for_host(&host);
    app.project = Ok((*project).clone());
    let id = app.tabs.focused();
    let bureau = &host.fleet().launch.bureau;
    let first = Pick {
        choice: Choice {
            account: Account::built_in("openrouter"),
            model: "record-first".into(),
            via: Some(Via::parse("deepinfra/fp8,novita").unwrap()),
        },
        tuning: Tuning {
            effort: Some(ReasoningEffort::High),
            ..Tuning::default()
        },
    };
    apply_model_switch(&mut app, id, first.clone());
    let first_commit = switch_commit(&host, id, |record| {
        matches!(record, Record::Control(Control::ModelChanged { .. }))
    });
    wait_for_pick(&host, id, &first);
    assert_eq!(status_model(&app, bureau), "openrouter/record-first (high)");
    assert!(
        !project.path().join("state.json").exists(),
        "neither asking nor publication saves a selection"
    );

    let second = Pick {
        choice: Choice {
            account: Account::built_in("deepseek"),
            model: "live-second".into(),
            via: None,
        },
        tuning: Tuning::default(),
    };
    apply_model_switch(&mut app, id, second.clone());
    let second_commit = switch_commit(&host, id, |record| {
        matches!(record, Record::Control(Control::ModelChanged { .. }))
    });
    wait_for_pick(&host, id, &second);
    assert_eq!(status_model(&app, bureau), "deepseek/live-second (auto)");
    assert!(!project.path().join("state.json").exists());

    app.commit(id, &first_commit);
    assert_eq!(
        state::load(&project),
        Some(state::State {
            provider: "openrouter".into(),
            provider_name: "openrouter".into(),
            model: "record-first".into(),
            via: first.choice.via,
        }),
        "the delivered record, not the newer live pick, decides every persisted field"
    );
    assert_eq!(status_model(&app, bureau), "deepseek/live-second (auto)");

    app.commit(id, &second_commit);
    assert_eq!(
        state::load(&project),
        Some(state::State {
            provider: "deepseek".into(),
            provider_name: "deepseek".into(),
            model: "live-second".into(),
            via: None,
        })
    );
    assert_eq!(host.end(), [] as [String; 0]);
}

#[test]
fn a_failed_switch_saves_nothing_even_when_its_live_error_commit_is_delivered() {
    let project = OwnDir::for_test("tui-failed-model").unwrap();
    let host = host(vec![]);
    let mut app = app_for_host(&host);
    app.project = Ok((*project).clone());
    let id = app.tabs.focused();
    let before = status_model(&app, &host.fleet().launch.bureau);
    apply_model_switch(
        &mut app,
        id,
        Pick {
            choice: Choice {
                account: Account::built_in("openrouter"),
                model: "unbuildable".into(),
                via: Some(Via::parse("novita").unwrap()),
            },
            tuning: Tuning::default(),
        },
    );
    let commit = switch_commit(&host, id, |record| {
        matches!(record,
            Record::Forensic(Forensic::Error { text }) if text.contains("could not switch to unbuildable")
        )
    });
    app.commit(id, &commit);
    assert!(!project.path().join("state.json").exists());
    assert_eq!(status_model(&app, &host.fleet().launch.bureau), before);
    assert_eq!(host.end(), [] as [String; 0]);
}

#[test]
fn births_and_reopens_do_not_persist_a_next_launch_selection() {
    use crate::record::{AgentLog, Born};
    let project = OwnDir::for_test("tui-birth-resume-selection").unwrap();
    let host = host(vec![]);
    let mut app = app_for_host(&host);
    app.project = Ok((*project).clone());
    let id = app.tabs.focused();
    let sessions = OwnDir::for_test("tui-selection-sessions").unwrap();
    let selected = crate::provider::Selected::for_test("born-model");
    let (log, _, _, _) =
        AgentLog::root(&sessions, id, "main", false, &selected, "system", None).unwrap();
    let born = Born {
        log_dir: log.dir().clone(),
        name: "main".into(),
        parent: None,
        resumed: None,
        reader: log.record_emitter().reader(),
    };
    app.born(id, &born);
    for commit in born.reader.since(None).unwrap().1 {
        app.commit(id, &commit.unwrap());
    }
    assert!(
        !project.path().join("state.json").exists(),
        "birth cannot pick the next launch's model"
    );
    let saved = state::State {
        provider: "openai".into(),
        provider_name: "existing choice".into(),
        model: "next-launch".into(),
        via: None,
    };
    state::save(&project, &saved).unwrap();
    let saved_bytes = project.read("state.json").unwrap();
    drop(log);
    let resumed_selected = crate::provider::Selected::for_test("reopened-model");
    let (log, _, _, _) = AgentLog::reopen(
        &sessions,
        id,
        &resumed_selected,
        false,
        "system",
        crate::app::now_unix_ms(),
    )
    .unwrap();
    let commits: Vec<_> = log
        .record_emitter()
        .reader()
        .since(None)
        .unwrap()
        .1
        .map(Result::unwrap)
        .collect();
    assert!(commits.iter().any(|commit| commit.records().iter().any(|record| matches!(record.value(), Record::Control(Control::SessionResumed { selected, .. }) if selected.model == "reopened-model"))));
    let reopened = Born {
        reader: log.record_emitter().reader(),
        log_dir: log.dir().clone(),
        resumed: Some(log.resumed_summary()),
        ..born
    };
    app.born(id, &reopened);
    for commit in &commits {
        app.commit(id, commit);
    }
    assert_eq!(
        project.read("state.json").unwrap(),
        saved_bytes,
        "reopen cannot overwrite the next launch's choice"
    );
    assert_eq!(state::load(&project), Some(saved));
    assert_eq!(host.end(), [] as [String; 0]);
}
