use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct LateFault {
    recorder: crate::record::Emitter,
    why: Option<String>,
    inner: Collecting,
    cleanup_pumping: mpsc::Sender<()>,
}

impl Sink for LateFault {
    fn drive(&mut self, rx: &crate::bus::BusReceiver, done: &AtomicBool) -> io::Result<()> {
        if self.why.is_some() {
            self.cleanup_pumping.send(()).unwrap();
            return self.inner.drive(rx, done);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "attendance did not settle");
            std::thread::yield_now();
        }
        self.recorder.fail_after(7);
        self.why = Some(
            self.recorder
                .emit(Display::Answer {
                    text: "late display".into(),
                })
                .unwrap_err()
                .to_string(),
        );
        self.inner.drive(rx, done)
    }
}

#[test]
fn a_recording_fault_after_attendance_overrides_failed_and_cancelled_jobs() {
    for cancelled in [false, true] {
        let root = converse_trunk(
            "late-recording-failure",
            Script::new().then(Reply::error(crate::provider::ProviderError::Other(
                "ordinary request failure".into(),
            ))),
        );
        let (cleanup_pumping, _pumped) = mpsc::channel();
        let mut sink = LateFault {
            recorder: root.recorder(),
            why: None,
            inner: Collecting::default(),
            cleanup_pumping,
        };
        let mut conversation = SettledConversation::new(root).unwrap();
        conversation.handle().steer("prompt".into()).unwrap();
        let job = conversation.await_job().unwrap();
        if cancelled {
            conversation.handle().end();
        }
        let ended = conversation.run(job, &mut sink);
        assert_eq!(
            sink.inner
                .transients
                .iter()
                .filter_map(|t| match t {
                    Transient::Fault { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [sink.why.as_deref().unwrap()]
        );
        assert_eq!(ended.outcome.unwrap_err(), sink.why.unwrap());
        assert!(conversation.root.check_available().is_err());
        assert!(!conversation.handle().is_live());
        assert!(conversation.handle().steer("no next job".into()).is_err());
    }
}

#[test]
fn late_recording_failure_pumps_replied_helper_teardown_before_job_end() {
    for failed_request in [false, true] {
        let script = if failed_request {
            (0..8).fold(Script::new(), |script, _| {
                script.then(Reply::error(crate::provider::ProviderError::Other(
                    "ordinary request failure".into(),
                )))
            })
        } else {
            parent_script()
        };
        let root = settled_trunk(
            "late-failure-helper",
            script,
            false,
            false,
            crate::agent::fleet::AGENT_LEASE_IDLE,
        );
        let mut independent = crate::agent::spawn::hatch_root(
            &root,
            crate::record::Memory::Amnemon,
            Some("independent".into()),
        )
        .unwrap();
        crate::agent::testkit::set_provider(
            &independent,
            crate::agent::testkit::scripted(
                "test-model",
                Script::new().then(Reply::text("independent work completed")),
            ),
        );
        let mut settled = SettledConversation::new(root).unwrap();
        let mut child = settled.root.fork_named("helper").unwrap();
        let helper = child.handle();
        let emit = settled.bus.emitter(settled.root.id());
        let child_emit = settled.bus.emitter(child.id());
        let provider = crate::agent::testkit::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![crate::agent::testkit::ral_call(
                "reply",
                "exarch-agents `reply 42",
            )])),
        );
        let outcome = child
            .deliberate(
                &DurableRoot::default().child(),
                &provider,
                Some(("help".into(), crate::record::Voice::Human)),
                None,
                &child_emit,
            )
            .unwrap();
        assert!(matches!(outcome, crate::agent::outcome::Outcome::Replied));
        assert!(helper.is_live());
        assert_eq!(helper.reply(), Some(FOValue::Int { value: 42 }));

        let (entered, tearing_down) = mpsc::channel();
        let (release, held) = mpsc::channel();
        child.engine.install_raw(Arc::new(TeardownGate {
            emit: child_emit,
            recorder: child.recorder(),
            entered,
            release: std::sync::Mutex::new(held),
        }));
        spawn_async(
            child,
            AsyncSpawn {
                name: "helper".into(),
                prompt: None,
                resumed: None,
            },
            &emit,
        )
        .unwrap();
        let (cleanup_pumping, pumped) = mpsc::channel();
        let mut sink = LateFault {
            recorder: settled.root.recorder(),
            why: None,
            inner: Collecting::default(),
            cleanup_pumping,
        };
        let handle = settled.handle();
        handle.steer("prompt".into()).unwrap();
        let job = settled.await_job().unwrap();
        let id = job.id();
        let (done, finished) = mpsc::channel();
        let driver = std::thread::spawn(move || {
            let end = settled.run(job, &mut sink);
            done.send((end, sink)).unwrap();
            settled
        });

        pumped
            .recv_timeout(Duration::from_secs(5))
            .expect("a late fault must start another output pump");
        tearing_down
            .recv_timeout(Duration::from_secs(5))
            .expect("terminal cleanup must cancel the replied helper");
        assert!(!handle.is_live());
        assert!(!helper.is_live());
        assert!(independent.handle().is_live());
        assert!(matches!(
            finished.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.send(()).unwrap();

        let (end, sink) = finished.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(end.id, id);
        assert!(!end.stopped);
        assert_eq!(
            sink.inner
                .transients
                .iter()
                .filter_map(|t| match t {
                    Transient::Fault { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [sink.why.as_deref().unwrap()]
        );
        assert_eq!(end.outcome.unwrap_err(), sink.why.unwrap());
        let output = sink.inner.signals.iter().position(|signal| matches!(signal,
            Signal::Fact(agent, rec) if *agent == helper.id() && matches!(rec.value(),
                Record::Display(Display::Answer { text }) if text == "helper teardown complete"
            )
        )).expect("helper's final output must reach the sink before JobEnd");
        let died = sink
            .inner
            .signals
            .iter()
            .position(|signal| {
                matches!(signal,
                    Signal::Transient(agent, Transient::Died) if *agent == helper.id()
                )
            })
            .expect("helper's death must reach the sink before JobEnd");
        assert!(output < died);
        let settled = driver.join().unwrap();
        assert!(settled.root.fleet.claim("helper").is_ok());
        assert!(settled.root.check_available().is_err());
        assert!(handle.steer("no next job".into()).is_err());
        assert!(
            settled.severance().is_none(),
            "recording failed, not transport"
        );

        independent.seed("continue independently".into());
        let (emit, _rx) = crate::bus::dummy_emitter();
        independent.attend(&emit);
        independent.check_available().unwrap();
        assert!(independent.handle().is_live());
        assert!(
            format!("{:?}", independent.rendered_messages()).contains("independent work completed")
        );
    }
}
