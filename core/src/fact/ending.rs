//! How a computation ended: the one type; every observer is a projection.

use super::{ErrorFact, ErrorRecord, Failure, Reason};
use crate::data::FOValue;
use crate::data::datum::{self, Datum};
use crate::fact::{DecodeCause, DecodeFailure, Expected, Found, ShapeFailure, UnknownTag};
use crate::process::CancelCause;
use crate::ty::{Ty, Typed, closed_variant};
use std::borrow::Cow;
use std::convert::Infallible;
use std::num::NonZeroU8;

/// The one table of ral tags, serde variant names and checker labels.
pub mod tag {
    pub const OK: &str = "ok";
    pub const ERR: &str = "err";
    pub const EXITED: &str = "exited";
    pub const STOPPED: &str = "stopped";
    pub const PANIC: &str = "panic";
    pub const ALL: [&str; 5] = [OK, ERR, EXITED, STOPPED, PANIC];
}

/// The status a panicked base reports: Rust's own.
pub const PANIC_CODE: u8 = 101;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ending<V, F, S> {
    Ok(V),
    Err(F),
    Exited(u8),
    Stopped(S),
    /// A defect, not a signal: re-raising resumes the unwind.
    Panic(String),
}

/// The process status of what can end in success. Enum matching allocates
/// and locks nothing: a signal handler may call it.
pub trait ExitCode {
    fn code(&self) -> u8;
}

impl<T: ExitCode + ?Sized> ExitCode for &T {
    fn code(&self) -> u8 {
        T::code(self)
    }
}

/// A failure's status: never the code that means success.
pub trait FailureCode {
    fn failure_code(&self) -> NonZeroU8;
}

impl<T: FailureCode + ?Sized> FailureCode for &T {
    fn failure_code(&self) -> NonZeroU8 {
        T::failure_code(self)
    }
}

impl<V, F, S> Ending<V, F, S> {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Ok(_) => tag::OK,
            Self::Err(_) => tag::ERR,
            Self::Exited(_) => tag::EXITED,
            Self::Stopped(_) => tag::STOPPED,
            Self::Panic(_) => tag::PANIC,
        }
    }

    pub fn map<V2, F2, S2>(
        self,
        v: impl FnOnce(V) -> V2,
        f: impl FnOnce(F) -> F2,
        s: impl FnOnce(S) -> S2,
    ) -> Ending<V2, F2, S2> {
        match self {
            Self::Ok(x) => Ending::Ok(v(x)),
            Self::Err(x) => Ending::Err(f(x)),
            Self::Exited(n) => Ending::Exited(n),
            Self::Stopped(x) => Ending::Stopped(s(x)),
            Self::Panic(t) => Ending::Panic(t),
        }
    }

    /// The ok/err half handed over as data, or the rest to re-raise.
    ///
    /// # Errors
    /// The exited, stopped or panicked ending, unchanged.
    pub fn outcome(self) -> Result<Result<V, F>, Ending<Infallible, Infallible, S>> {
        match self {
            Self::Ok(v) => Ok(Ok(v)),
            Self::Err(f) => Ok(Err(f)),
            Self::Exited(n) => Err(Ending::Exited(n)),
            Self::Stopped(s) => Err(Ending::Stopped(s)),
            Self::Panic(t) => Err(Ending::Panic(t)),
        }
    }
}

impl<S> Ending<Infallible, Infallible, S> {
    /// Back into any ending: the halts embed unchanged.
    pub fn widen<V, F>(self) -> Ending<V, F, S> {
        match self {
            Self::Ok(never) | Self::Err(never) => match never {},
            Self::Exited(n) => Ending::Exited(n),
            Self::Stopped(s) => Ending::Stopped(s),
            Self::Panic(t) => Ending::Panic(t),
        }
    }
}

impl<V, F: FailureCode, S: FailureCode> ExitCode for Ending<V, F, S> {
    fn code(&self) -> u8 {
        match self {
            Self::Ok(_) => 0,
            Self::Err(f) => f.failure_code().get(),
            Self::Exited(n) => *n,
            Self::Stopped(s) => s.failure_code().get(),
            Self::Panic(_) => PANIC_CODE,
        }
    }
}

impl<V, F, S> From<Result<V, F>> for Ending<V, F, S> {
    fn from(result: Result<V, F>) -> Self {
        match result {
            Ok(v) => Self::Ok(v),
            Err(f) => Self::Err(f),
        }
    }
}

impl<V: Datum, F: Datum, S: Datum> Datum for Ending<V, F, S> {
    fn encode(self) -> FOValue {
        let label = self.tag();
        let payload = match self {
            Self::Ok(v) => v.encode(),
            Self::Err(f) => f.encode(),
            Self::Exited(n) => n.encode(),
            Self::Stopped(s) => s.encode(),
            Self::Panic(t) => t.encode(),
        };
        datum::tag(label, Some(payload))
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let Some((label, payload)) = datum::untag(v) else {
            return Err(DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
                expected: Expected::Tags(tag::ALL.map(str::to_owned).to_vec()),
                found: Found::of(v),
            })));
        };
        let payload = payload.unwrap_or(&FOValue::Unit);
        match label {
            tag::OK => V::decode(payload).map(Self::Ok),
            tag::ERR => F::decode(payload).map(Self::Err),
            tag::EXITED => u8::decode(payload).map(Self::Exited),
            tag::STOPPED => S::decode(payload).map(Self::Stopped),
            tag::PANIC => String::decode(payload).map(Self::Panic),
            other => {
                return Err(DecodeFailure::new(DecodeCause::UnknownTag(UnknownTag {
                    tag: other.into(),
                    offered: tag::ALL.map(str::to_owned).to_vec(),
                })));
            }
        }
        .map_err(|why| why.payload(label))
    }
}

impl<V: Typed, F: Typed, S: Typed> Typed for Ending<V, F, S> {
    fn ty() -> Ty {
        closed_variant(&[
            (tag::OK, V::ty()),
            (tag::ERR, F::ty()),
            (tag::EXITED, Ty::int()),
            (tag::STOPPED, S::ty()),
            (tag::PANIC, Ty::string()),
        ])
    }
}

impl FailureCode for CancelCause {
    fn failure_code(&self) -> NonZeroU8 {
        match self {
            Self::ReaderGone => const { NonZeroU8::new(141).unwrap() },
            Self::Interrupted => const { NonZeroU8::new(130).unwrap() },
            Self::Cancelled | Self::Terminated => const { NonZeroU8::new(143).unwrap() },
            Self::TimedOut => const { NonZeroU8::new(124).unwrap() },
            Self::Aborted => const { NonZeroU8::new(131).unwrap() },
        }
    }
}

impl FailureCode for Reason {
    fn failure_code(&self) -> NonZeroU8 {
        match self {
            Self::Process(process) => process.failure.failure_code(),
            Self::TimedOut => CancelCause::TimedOut.failure_code(),
            Self::Cancelled => CancelCause::Cancelled.failure_code(),
            Self::Raised(claim) => claim.status,
            Self::Denied(_)
            | Self::Io(_)
            | Self::Invalid(_)
            | Self::Evaluation(_)
            | Self::State(_)
            | Self::Compile(_)
            | Self::Launch(_)
            | Self::Protocol(_)
            | Self::Host(_)
            | Self::Invariant(_) => NonZeroU8::MIN,
        }
    }
}

impl FailureCode for Failure {
    fn failure_code(&self) -> NonZeroU8 {
        use crate::process::outcome::failure_byte;
        match self {
            Self::Exited(n) => failure_byte(*n),
            Self::Signaled(n) => failure_byte(128_i32.saturating_add(*n)),
            Self::NotFound => const { NonZeroU8::new(127).unwrap() },
            Self::NotRunnable(_) => const { NonZeroU8::new(126).unwrap() },
        }
    }
}

impl FailureCode for ErrorFact {
    fn failure_code(&self) -> NonZeroU8 {
        self.reason.failure_code()
    }
}

impl FailureCode for ErrorRecord {
    fn failure_code(&self) -> NonZeroU8 {
        self.status
    }
}

/// A worker's ending: what `done` carries, `workers` lists and hosts keep.
pub type Done = Ending<(), ErrorRecord, CancelCause>;

impl Done {
    /// The head and its detail; a failure's detail is its `message`.
    pub fn parts(&self) -> (Cow<'static, str>, Option<String>) {
        match self {
            Self::Ok(()) => ("returned".into(), None),
            Self::Err(f) => ("failed".into(), Some(f.message.clone())),
            Self::Exited(n) => (format!("ran `exit {n}`").into(), None),
            Self::Stopped(s) => (s.message().into(), None),
            Self::Panic(t) => ("panicked".into(), Some(t.clone())),
        }
    }

    /// `head` or `head: detail`: the one line every host shows.
    pub fn said(&self) -> String {
        let (head, detail) = self.parts();
        match detail {
            Some(detail) => format!("{head}: {detail}"),
            None => head.into_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::Reason;
    use crate::ty::TyNode;
    use strum::VariantArray as _;

    fn failure() -> ErrorRecord {
        ErrorFact::new(
            Reason::raised("boom"),
            "fail".into(),
            crate::test_helper::site(),
        )
        .record()
    }

    fn dones() -> [Done; 5] {
        [
            Done::Ok(()),
            Done::Err(failure()),
            Done::Exited(3),
            Done::Stopped(CancelCause::TimedOut),
            Done::Panic("kaput".into()),
        ]
    }

    #[test]
    fn every_ending_has_its_code() {
        for (ending, code) in dones().into_iter().zip([0, 1, 3, 124, PANIC_CODE]) {
            assert_eq!(ending.code(), code);
        }
        for (cause, code) in [
            (CancelCause::Interrupted, 130),
            (CancelCause::Cancelled, 143),
            (CancelCause::TimedOut, 124),
            (CancelCause::Terminated, 143),
            (CancelCause::Aborted, 131),
            (CancelCause::ReaderGone, 141),
        ] {
            assert_eq!(cause.failure_code().get(), code, "{cause:?}");
        }
    }

    #[test]
    fn a_worker_converts_only_a_cancel_or_a_timeout() {
        for cause in CancelCause::VARIANTS.iter().copied() {
            let reason = Reason::stopped(cause);
            match cause {
                CancelCause::Cancelled | CancelCause::TimedOut => {
                    let reason = reason.expect("a worker's own stop");
                    assert_eq!(reason.failure_code(), cause.failure_code());
                    assert_eq!(reason.describe(), cause.message());
                }
                _ => assert_eq!(reason, None, "{cause:?} is the host's"),
            }
        }
    }

    #[test]
    fn the_tags_are_one_table() {
        for ending in dones() {
            let label = ending.tag();
            let encoded = ending.encode();
            assert_eq!(datum::untag(&encoded).map(|(label, _)| label), Some(label));
        }
        let ty = Done::ty();
        let TyNode::Variant(row) = ty.node() else {
            panic!("an ending has a variant type");
        };
        assert_eq!(
            row.fields()
                .map(|(label, _)| label.name())
                .collect::<Vec<_>>(),
            tag::ALL
        );
        assert!(!row.is_open());
    }

    #[test]
    fn dones_ok_is_payload_less() {
        let encoded = Done::Ok(()).encode();
        assert_eq!(encoded, datum::tag(tag::OK, None));
        assert_eq!(Done::decode(&encoded), Ok(Done::Ok(())));
        let explicit_unit = FOValue::Variant {
            label: tag::OK.into(),
            payload: Some(Box::new(FOValue::Unit)),
        };
        assert_eq!(Done::decode(&explicit_unit), Ok(Done::Ok(())));
    }

    #[test]
    fn done_round_trips() {
        #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        struct Stored {
            #[serde(with = "datum::as_datum")]
            ending: Done,
        }

        for ending in dones() {
            assert_eq!(Done::decode(&ending.clone().encode()), Ok(ending.clone()));
            let stored = Stored { ending };
            let json = serde_json::to_string(&stored).unwrap();
            assert_eq!(serde_json::from_str::<Stored>(&json).unwrap(), stored);
        }
    }

    #[test]
    fn the_wire_form_is_external_kebab() {
        let ending = Ending::<(), (), CancelCause>::Stopped(CancelCause::TimedOut);
        assert_eq!(
            serde_json::to_string(&ending).unwrap(),
            r#"{"stopped":"timed-out"}"#
        );
        assert_eq!(
            serde_json::from_str::<Ending<(), (), CancelCause>>(r#"{"stopped":"timed-out"}"#)
                .unwrap(),
            ending
        );
    }

    #[test]
    fn a_workers_ending_in_words() {
        for (ending, said) in dones().into_iter().zip([
            "returned",
            "failed: boom",
            "ran `exit 3`",
            "timed out",
            "panicked: kaput",
        ]) {
            assert_eq!(ending.said(), said);
        }
        assert_eq!(
            Done::Err(failure()).parts(),
            ("failed".into(), Some("boom".into()))
        );
    }
}
