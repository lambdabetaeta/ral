//! The laws every carrier shares: the event stream's stash and the
//! severance codes.
use super::*;

#[allow(clippy::disallowed_methods, reason = "test scaffolding")]
mod event_receiver_tests {
    use super::*;

    fn receiver() -> (EventReceiver, mpsc::Sender<(DispatchId, Event)>) {
        let (tx, rx) = mpsc::channel();
        (EventReceiver::new(rx), tx)
    }

    /// The stash precedence `recv` gives, proven on the non-blocking door.
    #[test]
    fn try_recv_drains_the_stash_before_the_channel() {
        let (receiver, tx) = receiver();
        let channelled = (DispatchId(2), Event::Surface(FOValue::Unit));
        let stashed = (DispatchId(1), Event::Surface(FOValue::Unit));
        tx.send(channelled.clone()).unwrap();
        receiver.stash.lock().unwrap().push_back(stashed.clone());

        assert_eq!(receiver.try_recv(), Some(stashed));
        assert_eq!(receiver.try_recv(), Some(channelled));
    }

    /// Empty on both sides returns `None` rather than blocking.
    #[test]
    fn try_recv_returns_none_when_empty() {
        let (receiver, _tx) = receiver();
        assert_eq!(receiver.try_recv(), None);
    }
}

/// The severance codes are load-bearing text: a front-end prints them beside
/// its one-sentence failure so that a person who cannot read a log still has
/// something exact to quote.  These assertions exist to make a rename a
/// deliberate act with a failing test attached, rather than a tidy-up nobody
/// notices until a support thread stops matching.
mod severance_codes {
    use super::*;

    #[test]
    fn every_severance_has_its_own_settled_code() {
        let codes = [
            (Severed::Refused("v9".into()).code(), "engine-refused"),
            (Severed::Closed("eof".into()).code(), "engine-closed"),
            (
                Severed::Silent(Duration::from_secs(30)).code(),
                "engine-silent",
            ),
            (Severed::Faulted("junk".into()).code(), "engine-faulted"),
        ];
        for (got, want) in codes {
            assert_eq!(got, want, "a severance code may not be renamed in place");
        }
        let mut distinct: Vec<_> = codes.iter().map(|(got, _)| *got).collect();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), codes.len(), "two severances share a code");
    }
}
