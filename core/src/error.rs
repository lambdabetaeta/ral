//! The one error type: a record (`reason`, `prov`) plus the report of one
//! unwinding.  Rendering lives in `shell::runtime::error`.

use crate::diagnostic::Rejection;
use crate::fact::{FailureCode, IoFailure, IoOperation, IoSubject, Reason};
use crate::source::{CallSite, SourceDb, Span};
use std::borrow::Cow;
use std::fmt;
use std::io;
use std::num::NonZeroU8;
use std::sync::Arc;

/// Rides in every `Settled`, so its optional parts are boxed: clippy's
/// `result_large_err` refuses an `Err` of 128 bytes.
#[derive(Debug, Clone)]
pub struct Error {
    pub reason: Reason,
    pub prov: Option<Prov>,
    hints: Box<[Box<str>]>,
    witness: Option<Box<Span>>,
    rejection: Option<Box<Rejection>>,
    context: Option<Box<str>>,
    unwound: Unwound,
}

/// Where a failure was claimed.  `Live` is resolved against a source registry
/// only when drawn; `Resolved` carries its coordinates and renders no caret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prov {
    Live { name: FrameName, span: Span },
    Resolved(Box<(String, CallSite)>),
}

/// A frame's name.  Static spellings and shared [`crate::ir::Name`]s clone
/// without allocating, which the evaluator's per-step register depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameName {
    Static(&'static str),
    Shared(Arc<str>),
}

impl std::ops::Deref for FrameName {
    type Target = str;

    fn deref(&self) -> &str {
        match self {
            Self::Static(name) => name,
            Self::Shared(name) => name,
        }
    }
}

impl From<&'static str> for FrameName {
    fn from(name: &'static str) -> Self {
        Self::Static(name)
    }
}

impl From<Arc<str>> for FrameName {
    fn from(name: Arc<str>) -> Self {
        Self::Shared(name)
    }
}

impl From<&Arc<str>> for FrameName {
    fn from(name: &Arc<str>) -> Self {
        Self::Shared(Arc::clone(name))
    }
}

impl From<Cow<'static, str>> for FrameName {
    fn from(name: Cow<'static, str>) -> Self {
        match name {
            Cow::Borrowed(name) => Self::Static(name),
            Cow::Owned(name) => name.into(),
        }
    }
}

impl From<String> for FrameName {
    fn from(name: String) -> Self {
        Self::Shared(name.into())
    }
}

impl Prov {
    pub fn name(&self) -> &str {
        match self {
            Self::Live { name, .. } => name,
            Self::Resolved(pair) => &pair.0,
        }
    }

    pub fn span(&self) -> Option<Span> {
        match self {
            Self::Live { span, .. } => Some(*span),
            Self::Resolved(_) => None,
        }
    }

    pub(crate) fn site(&self, db: &SourceDb) -> CallSite {
        match self {
            Self::Live { span, .. } => db.site(*span).expect("live provenance is registered"),
            Self::Resolved(pair) => pair.1.clone(),
        }
    }

    /// Leaving the shell `db` belongs to: a span into a file the parent
    /// registry does not hold (or holds as another file) becomes coordinates.
    pub(crate) fn leaving(self, db: &SourceDb) -> Self {
        match self {
            Self::Live { name, span } if !db.inherits(span.file) => match db.site(span) {
                Some(site) => Self::Resolved(Box::new((name.to_string(), site))),
                None => Self::Live { name, span },
            },
            prov => prov,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Unwound {
    pub abandoned_tail: bool,
    pub discarded: u32,
}

const _: () = assert!(
    std::mem::size_of::<Error>() < 128,
    "Error must stay under clippy's result_large_err threshold"
);

impl FailureCode for Error {
    fn failure_code(&self) -> NonZeroU8 {
        self.reason.failure_code()
    }
}

impl Error {
    /// Construct a failure from portable facts, never implicit prose.
    ///
    /// ```
    /// use ral_core::error::Error;
    /// use ral_core::fact::{Reason, StateFailure};
    ///
    /// let error = Error::new(Reason::State(Box::new(StateFailure::NoDesk)));
    /// assert!(matches!(error.reason, Reason::State(cause) if *cause == StateFailure::NoDesk));
    /// ```
    ///
    /// Neither borrowed nor owned strings are reasons:
    ///
    /// ```compile_fail,E0308
    /// use ral_core::error::Error;
    ///
    /// let _ = Error::new("not a typed reason");
    /// ```
    ///
    /// ```compile_fail,E0308
    /// use ral_core::error::Error;
    ///
    /// let _ = Error::new(String::from("not a typed reason"));
    /// ```
    ///
    /// Strings cannot implicitly convert to a reason either:
    ///
    /// ```compile_fail,E0277
    /// use ral_core::error::Error;
    /// use ral_core::fact::Reason;
    ///
    /// let reason: Reason = "not a typed reason".into();
    /// let _ = Error::new(reason);
    /// ```
    pub fn new(reason: Reason) -> Self {
        Self {
            reason,
            prov: None,
            hints: Box::default(),
            witness: None,
            rejection: None,
            context: None,
            unwound: Unwound::default(),
        }
    }

    /// Project I/O at its source boundary, preserving custom path/owned causes.
    pub fn io(operation: IoOperation, subject: IoSubject, error: &io::Error) -> Self {
        Self::new(Reason::Io(Box::new(IoFailure {
            operation,
            subject,
            cause: crate::io::failure(error),
        })))
    }

    /// The failure's own provenance, else the register's frame.
    pub(crate) fn prov_or<'a>(&'a self, register: &'a Prov) -> &'a Prov {
        self.prov.as_ref().unwrap_or(register)
    }

    pub fn describe(&self) -> Cow<'_, str> {
        self.reason.describe()
    }

    /// Stored advice, innermost site first.
    pub fn hints(&self) -> &[Box<str>] {
        &self.hints
    }

    pub fn context(&self) -> Option<&str> {
        self.context.as_deref()
    }

    pub fn witness(&self) -> Option<Span> {
        self.witness.as_deref().copied()
    }

    pub fn rejection(&self) -> Option<&Rejection> {
        self.rejection.as_deref()
    }

    pub fn unwound(&self) -> Unwound {
        self.unwound
    }

    pub fn with_hint(self, hint: impl Into<Box<str>>) -> Self {
        self.with_hints([hint])
    }

    /// Each of `hints`, in order: an `Option` adds its hint if it has one.
    pub fn with_hints(mut self, hints: impl IntoIterator<Item: Into<Box<str>>>) -> Self {
        let mut all = std::mem::take(&mut self.hints).into_vec();
        all.extend(hints.into_iter().map(Into::into));
        self.hints = all.into_boxed_slice();
        self
    }

    /// Resolve local provenance and drop witnesses the parent cannot read.
    pub(crate) fn leaving(mut self, sources: &SourceDb) -> Self {
        self.prov = self.prov.map(|prov| prov.leaving(sources));
        self.witness = self.witness.filter(|span| sources.inherits(span.file));
        self
    }

    pub(crate) fn with_witness(mut self, witness: Option<Span>) -> Self {
        self.witness = witness.map(Box::new);
        self
    }

    pub(crate) fn with_rejection(mut self, rejection: Rejection) -> Self {
        self.rejection = Some(Box::new(rejection));
        self
    }

    pub fn with_context(mut self, ctx: impl fmt::Display) -> Self {
        self.context = Some(match self.context.take() {
            Some(inner) => format!("{ctx}: {inner}").into(),
            None => ctx.to_string().into(),
        });
        self
    }

    pub(crate) fn abandoning_tail(mut self) -> Self {
        self.unwound.abandoned_tail = true;
        self
    }

    pub(crate) fn discarded(mut self, bytes: u32) -> Self {
        self.unwound.discarded = self.unwound.discarded.saturating_add(bytes);
        self
    }
}

/// The message line of a report: the context chain, then the record's words.
pub(crate) fn under_context(context: Option<&str>, message: &str) -> String {
    match context {
        Some(context) => format!("{context}: {message}"),
        None => message.to_owned(),
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&under_context(self.context(), &self.describe()))
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    fn said(e: &io::Error) -> String {
        Error::io(IoOperation::Read, IoSubject::Path("\"f\"".into()), e)
            .describe()
            .into_owned()
    }

    /// `NotFound` and `PermissionDenied` say the fixed string, whoever made
    /// them and whatever words they carry.
    #[test]
    fn io_says_the_fixed_string_for_not_found_and_permission_denied() {
        let os_not_found = io::Error::from_raw_os_error(2);
        assert_eq!(os_not_found.kind(), ErrorKind::NotFound);
        assert_eq!(
            said(&os_not_found),
            "could not read \"f\": no such file or directory"
        );
        assert_eq!(
            said(&ErrorKind::PermissionDenied.into()),
            "could not read \"f\": permission denied"
        );
        assert_eq!(
            said(&ErrorKind::NotFound.into()),
            "could not read \"f\": no such file or directory"
        );
        assert_eq!(
            said(&io::Error::new(ErrorKind::PermissionDenied, "other words")),
            "could not read \"f\": permission denied"
        );
        assert_eq!(
            said(&io::Error::new(ErrorKind::NotFound, "other words")),
            "could not read \"f\": no such file or directory"
        );
    }

    /// The directory's refusal of a staged write says whose refusal it is.
    #[test]
    fn io_says_the_directorys_refusal_in_its_own_words() {
        assert_eq!(
            said(&crate::path::directory_refusal_for_test()),
            "could not read \"f\": its directory is not writable"
        );
    }
}
