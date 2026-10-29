//! The gated job loop: each settled job is walked and certified before the next is released.

use crate::wire::{JobId, WindowReport};
use exarch::bus::{BusReceiver, Signal, Sink};
use exarch::record::{AgentId, Forensic, Record, Transient};
use ral_core::carrier::Severed;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(100);

/// The conversation, as the loop drives it.
pub(crate) trait Seat {
    fn root(&self) -> AgentId;
    fn feed(&self) -> &BusReceiver;
    /// The window's sentence for a lost engine.
    fn engine_lost(&self, cause: &Severed) -> String;
    /// Admit the next job.
    fn release(&self) -> Result<(), String>;
    /// Walk after a settled job; the report replaces the last.
    fn settled(&mut self) -> Result<(), String>;
    fn report(&self) -> Option<WindowReport>;
    fn invalidate(&mut self);
}

/// The window, as the loop publishes to it.
pub(crate) trait Stage {
    /// Whether this conversation still owns the window.
    fn current(&self) -> bool;
    fn failure(&self, message: String);
    /// False once the window has moved on.
    fn job_began(&self, job: JobId) -> bool;
    fn job_done(&self, job: JobId, stopped: bool, report: Option<WindowReport>);
}

/// Release the first job, then walk and release after each settlement until the
/// conversation ends. `Err(explained)` says whether a failure was already shown.
pub(crate) fn run(
    seat: &mut impl Seat,
    stage: &impl Stage,
    sink: &mut impl Sink,
) -> Result<(), bool> {
    if let Err(message) = seat.release() {
        stage.failure(message);
        seat.invalidate();
        return Err(true);
    }

    let root = seat.root();
    loop {
        let sig = match seat.feed().recv_timeout(POLL) {
            Ok(sig) => sig,
            Err(RecvTimeoutError::Timeout) => {
                if !stage.current() {
                    return Ok(());
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                if stage.current() {
                    stage.failure("The assistant's event feed disconnected.".into());
                    seat.invalidate();
                    return Err(true);
                }
                return Ok(());
            }
        };
        if let Some(message) = terminal_failure(&sig, root, |cause| seat.engine_lost(cause)) {
            sink.accept(sig);
            stage.failure(message);
            seat.invalidate();
            return Err(true);
        }
        let root_died =
            matches!(&sig, Signal::Transient(id, Transient::Died { .. }) if *id == root);
        let mut settlement = None;
        if let Signal::Commit(id, commit) = &sig
            && *id == root
        {
            for rec in commit.records() {
                match rec.value() {
                    Record::Forensic(Forensic::JobOpened) => {
                        if !stage.job_began(exarch::record::JobId::from(commit.number()).into()) {
                            return Ok(());
                        }
                    }
                    Record::Forensic(Forensic::JobSettled { job, stopped }) => {
                        settlement = Some((*job, *stopped));
                    }
                    _ => {}
                }
            }
        }
        sink.accept(sig);
        if root_died {
            return if stage.current() { Err(false) } else { Ok(()) };
        }
        if let Some((job, stopped)) = settlement {
            if let Some(message) = drain_failures(seat, root, sink) {
                stage.failure(message);
                seat.invalidate();
                return Err(true);
            }
            let walk = seat.settled();
            if let Err(message) = finish_walk(
                walk,
                || drain_failures(seat, root, sink),
                |message| stage.failure(message),
                || stage.job_done(job.into(), stopped, seat.report()),
                || seat.release(),
            ) {
                stage.failure(message);
                seat.invalidate();
                return Err(true);
            }
        }
    }
}

fn finish_walk(
    walk: Result<(), String>,
    certify: impl FnOnce() -> Option<String>,
    failure: impl FnOnce(String),
    done: impl FnOnce(),
    release: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    if let Some(message) = certify() {
        return Err(message);
    }
    if let Err(message) = walk {
        failure(message);
    }
    done();
    release()
}

fn terminal_failure(
    sig: &Signal,
    root: AgentId,
    engine_lost: impl Fn(&Severed) -> String,
) -> Option<String> {
    match sig {
        Signal::Commit(id, commit) => commit.records().iter().find_map(|rec| match rec.value() {
            Record::Forensic(Forensic::TeardownFailed { failure }) => Some(failure.clone()),
            Record::Forensic(Forensic::EngineSevered { cause }) if *id == root => {
                Some(engine_lost(cause))
            }
            _ => None,
        }),
        Signal::Transient(_, Transient::Died { failures }) if !failures.is_empty() => {
            Some(failures.join("\n"))
        }
        _ => None,
    }
}

fn drain_failures(seat: &impl Seat, root: AgentId, sink: &mut impl Sink) -> Option<String> {
    snapshot_failure(seat.feed().drain_pending(), root, sink, |cause| {
        seat.engine_lost(cause)
    })
}

fn snapshot_failure(
    signals: Vec<Signal>,
    root: AgentId,
    sink: &mut impl Sink,
    engine_lost: impl Fn(&Severed) -> String,
) -> Option<String> {
    let mut failure = None;
    for signal in signals {
        if failure.is_none() {
            failure = terminal_failure(&signal, root, &engine_lost);
            if failure.is_none()
                && matches!(&signal, Signal::Transient(id, Transient::Died { .. }) if *id == root)
            {
                failure = Some("The assistant ended before its job could be certified.".into());
            }
        }
        sink.accept(signal);
    }
    failure
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::test_fixture::scripted_host;
    use exarch::provider::scripted::{Reply, Script};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    struct Ignore;
    impl Sink for Ignore {}

    type Log = Rc<RefCell<Vec<String>>>;

    struct FakeSeat<'a> {
        feed: &'a BusReceiver,
        root: AgentId,
        log: Log,
        /// Whether the root's gate admits a job.
        admit: Box<dyn Fn() -> bool + 'a>,
    }

    impl<'a> FakeSeat<'a> {
        fn new(feed: &'a BusReceiver, log: &Log, admit: impl Fn() -> bool + 'a) -> Self {
            Self {
                feed,
                root: AgentId::new(1),
                log: log.clone(),
                admit: Box::new(admit),
            }
        }
    }

    impl Seat for FakeSeat<'_> {
        fn root(&self) -> AgentId {
            self.root
        }
        fn feed(&self) -> &BusReceiver {
            self.feed
        }
        fn engine_lost(&self, _: &Severed) -> String {
            "engine".into()
        }
        fn release(&self) -> Result<(), String> {
            self.log.borrow_mut().push("release".into());
            if (self.admit)() {
                Ok(())
            } else {
                Err("refused".into())
            }
        }
        fn settled(&mut self) -> Result<(), String> {
            self.log.borrow_mut().push("walk".into());
            Ok(())
        }
        fn report(&self) -> Option<WindowReport> {
            None
        }
        fn invalidate(&mut self) {
            self.log.borrow_mut().push("invalidate".into());
        }
    }

    struct FakeStage {
        log: Log,
        current: Rc<Cell<bool>>,
        after_done: Box<dyn Fn(usize)>,
    }

    impl FakeStage {
        fn new(log: &Log) -> Self {
            Self {
                log: log.clone(),
                current: Rc::new(Cell::new(true)),
                after_done: Box::new(|_| {}),
            }
        }
    }

    impl Stage for FakeStage {
        fn current(&self) -> bool {
            self.current.get()
        }
        fn failure(&self, message: String) {
            self.log.borrow_mut().push(format!("failure: {message}"));
        }
        fn job_began(&self, _: JobId) -> bool {
            self.log.borrow_mut().push("began".into());
            true
        }
        fn job_done(&self, _: JobId, stopped: bool, _: Option<WindowReport>) {
            let done = {
                let mut log = self.log.borrow_mut();
                log.push(format!("done stopped={stopped}"));
                log.iter().filter(|e| e.starts_with("done")).count()
            };
            (self.after_done)(done);
        }
    }

    #[test]
    fn each_release_admits_one_job_and_each_settlement_is_walked_before_the_next() {
        let run_dir = ral_core::own::OwnDir::for_test("gate-two-jobs").expect("run directory");
        let host = scripted_host(
            &run_dir,
            None,
            Script::new()
                .then(Reply::text("one"))
                .then(Reply::text("two")),
        );
        let root = host.roots()[0].clone();
        root.steer("first".into()).expect("live root");
        let log = Log::default();
        let mut seat = FakeSeat::new(host.feed(), &log, || root.release());
        seat.root = root.id();
        let mut stage = FakeStage::new(&log);
        let current = stage.current.clone();
        let prompter = root.clone();
        // The second prompt arrives once the first job is reported; the second report ends the run.
        stage.after_done = Box::new(move |done| match done {
            1 => prompter
                .steer("second".into())
                .expect("a settled root takes a prompt"),
            _ => current.set(false),
        });

        assert_eq!(run(&mut seat, &stage, &mut Ignore), Ok(()));

        assert_eq!(
            *log.borrow(),
            [
                "release",
                "began",
                "walk",
                "done stopped=false",
                "release",
                "began",
                "walk",
                "done stopped=false",
                "release",
            ]
        );
        drop(seat);
        assert_eq!(host.end(), Vec::<String>::new());
    }

    #[test]
    fn a_refused_first_release_fails_before_any_job_begins() {
        let (_tx, rx) = exarch::bus::channel();
        let log = Log::default();
        let mut seat = FakeSeat::new(&rx, &log, || false);
        assert_eq!(
            run(&mut seat, &FakeStage::new(&log), &mut Ignore),
            Err(true)
        );
        assert_eq!(*log.borrow(), ["release", "failure: refused", "invalidate"]);
    }

    #[test]
    fn a_root_death_ends_quietly_only_when_the_window_has_moved_on() {
        for (current, expected) in [(true, Err(false)), (false, Ok(()))] {
            let (tx, rx) = exarch::bus::channel();
            let log = Log::default();
            let mut seat = FakeSeat::new(&rx, &log, || true);
            let stage = FakeStage::new(&log);
            stage.current.set(current);
            tx.send_signal(Signal::Transient(
                seat.root,
                Transient::Died { failures: vec![] },
            ))
            .expect("feed is open");
            assert_eq!(run(&mut seat, &stage, &mut Ignore), expected);
            assert_eq!(*log.borrow(), ["release"]);
        }
    }

    #[test]
    fn a_disconnected_feed_is_a_failure_while_the_window_is_current() {
        let (tx, rx) = exarch::bus::channel();
        drop(tx);
        let log = Log::default();
        let mut seat = FakeSeat::new(&rx, &log, || true);
        assert_eq!(
            run(&mut seat, &FakeStage::new(&log), &mut Ignore),
            Err(true)
        );
        assert_eq!(
            *log.borrow(),
            [
                "release",
                "failure: The assistant's event feed disconnected.",
                "invalidate"
            ]
        );
    }

    #[test]
    fn failure_during_a_blocked_walk_is_seen_by_the_post_walk_snapshot() {
        let (tx, rx) = exarch::bus::channel();
        let root = AgentId::new(1);
        let helper = AgentId::new(2);
        assert!(
            snapshot_failure(rx.drain_pending(), root, &mut Ignore, |_| "engine".into()).is_none()
        );
        let (walking, ready) = std::sync::mpsc::channel();
        let (continue_walk, finish) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            walking.send(()).unwrap();
            finish.recv().unwrap();
            snapshot_failure(rx.drain_pending(), root, &mut Ignore, |_| "engine".into())
        });
        ready.recv().unwrap();
        tx.send_signal(Signal::Transient(
            helper,
            Transient::Died {
                failures: vec!["uncertified".into()],
            },
        ))
        .unwrap();
        continue_walk.send(()).unwrap();
        assert_eq!(worker.join().unwrap().as_deref(), Some("uncertified"));
    }

    #[test]
    fn only_terminal_verdicts_invalidate_a_snapshot() {
        let root = AgentId::new(1);
        let helper = AgentId::new(2);
        let signals = vec![
            Signal::Transient(
                helper,
                Transient::Elided {
                    text: "presentation elided".into(),
                },
            ),
            Signal::Transient(helper, Transient::Died { failures: vec![] }),
        ];
        assert!(snapshot_failure(signals, root, &mut Ignore, |_| "engine".into()).is_none());
    }

    #[test]
    fn an_unavailable_walk_publishes_failure_then_done_and_releases_for_retry() {
        let events = RefCell::new(Vec::new());
        assert_eq!(
            finish_walk(
                Err("walk failed".into()),
                || {
                    events.borrow_mut().push("certify".into());
                    None
                },
                |message| events.borrow_mut().push(message),
                || events.borrow_mut().push("done without report".into()),
                || {
                    events.borrow_mut().push("release".into());
                    Ok(())
                },
            ),
            Ok(())
        );
        assert_eq!(
            *events.borrow(),
            ["certify", "walk failed", "done without report", "release"]
        );
    }

    #[test]
    fn a_failed_walk_still_checks_certification_before_done_or_release() {
        let events = RefCell::new(Vec::new());
        let result = finish_walk(
            Err("walk failed".into()),
            || {
                events.borrow_mut().push("certify");
                Some("helper failed".into())
            },
            |_| events.borrow_mut().push("walk failure"),
            || events.borrow_mut().push("done"),
            || {
                events.borrow_mut().push("release");
                Ok(())
            },
        );
        assert_eq!(result, Err("helper failed".into()));
        assert_eq!(*events.borrow(), ["certify"]);
    }
}
