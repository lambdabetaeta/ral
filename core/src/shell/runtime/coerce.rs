//! The decoders of a builtin's arguments.

use super::{Break, Env, Error, ListRef, MapRef, ValClos, View};
use crate::fact::{
    BlockRole, CodecFailure, CodecFailureUtf8, Expected, IllTyped, InvalidCause, InvalidFailure,
    Reason, ShapeFailure, Utf8Failure, Utf8Subject, ValueShape,
};
use crate::handle::HandleInner;
use crate::ir::Comp;
use std::sync::Arc;

/// Each decoder fails with a bare `Error`: a shape mismatch is never an exit.
impl ValClos {
    /// A shape refusal with a semantic operation, not a diagnostic headline.
    pub fn expected(&self, operation: &str, expected: Expected) -> Error {
        invalid(
            operation,
            InvalidCause::Shape(ShapeFailure {
                expected,
                found: self.found(),
            }),
        )
    }

    /// Borrow a `String` argument without copying it.
    ///
    /// # Errors
    /// `self` is not a `String`.
    pub fn as_str(&self, ctx: &str) -> Result<&str, Error> {
        match self.view() {
            View::String(s) => Ok(s.as_str()),
            _ => Err(self.expected(ctx, Expected::Shape(ValueShape::String))),
        }
    }

    /// # Errors
    /// `self` is not `Bytes`.
    pub(crate) fn as_bytes(&self, ctx: &str) -> Result<&[u8], Error> {
        match self.view() {
            View::Bytes(b) => Ok(b),
            _ => Err(self.expected(ctx, Expected::Shape(ValueShape::Bytes))),
        }
    }

    /// # Errors
    /// `self` is not a `Handle`.
    pub(crate) fn expect_handle(&self, ctx: &str) -> Result<&HandleInner, Error> {
        match self.view() {
            View::Handle(h) => h.live(),
            _ => Err(self.expected(ctx, Expected::Shape(ValueShape::Handle))),
        }
    }

    /// A spawn or watch body: a block, which the checker holds to `U (F A)`.
    ///
    /// # Errors
    /// `self` is not a block.
    pub(crate) fn expect_thunk(&self, ctx: &str) -> Result<(&Arc<Comp>, &Env), Error> {
        match self.view() {
            View::Thunk { comp, env } => Ok((comp, env)),
            _ => Err(self.expected(ctx, Expected::Closure)),
        }
    }

    /// # Errors
    /// `self` is not a `List`.
    pub fn as_list(&self, ctx: &str) -> Result<ListRef<'_>, Error> {
        match self.view() {
            View::List(items) => Ok(items),
            _ => Err(self.expected(ctx, Expected::Shape(ValueShape::List))),
        }
    }

    /// # Errors
    /// `self` is not a `Map`.
    pub fn as_map(&self, ctx: &str) -> Result<MapRef<'_>, Error> {
        match self.view() {
            View::Map(m) => Ok(m),
            _ => Err(self.expected(ctx, Expected::Shape(ValueShape::Map))),
        }
    }
}

/// An invalid input at its owning operation.
pub(crate) fn invalid(operation: &str, cause: InvalidCause) -> Error {
    Error::new(Reason::Invalid(Box::new(InvalidFailure::new(
        operation.into(),
        cause,
    ))))
}

/// `role`, a block the checker refused with `errors`, at `operation`.
pub(crate) fn ill_typed(
    operation: &str,
    role: BlockRole,
    errors: &[crate::typecheck::TypeError],
) -> Error {
    invalid(
        operation,
        InvalidCause::IllTyped(
            IllTyped::new(
                role,
                errors
                    .iter()
                    .map(crate::typecheck::TypeError::diagnostic)
                    .collect(),
            )
            .expect("the checker refuses a block only with nonempty diagnostics"),
        ),
    )
}

/// # Errors
/// `bytes` is not valid UTF-8.
pub(crate) fn decode_utf8_strict(
    bytes: Vec<u8>,
    operation: &str,
    subject: Utf8Subject,
) -> Result<String, Break> {
    String::from_utf8(bytes).map_err(|error| {
        let cause = error.utf8_error();
        invalid(
            operation,
            InvalidCause::Codec(CodecFailure::Utf8(CodecFailureUtf8 {
                subject,
                cause: Utf8Failure {
                    valid_up_to: cause.valid_up_to().into(),
                    error_len: cause.error_len().map(Into::into),
                },
            })),
        )
        .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::{Count, Found};

    #[test]
    fn coercions_keep_typed_expectations_and_bounded_previews() {
        let value = ValClos::string("é".repeat(25));
        let error = value.as_list("map").unwrap_err();
        let Reason::Invalid(failure) = error.reason else {
            panic!("invalid input")
        };
        assert_eq!(failure.operation, "map");
        assert_eq!(
            failure.cause,
            InvalidCause::Shape(ShapeFailure {
                expected: Expected::Shape(ValueShape::List),
                found: value.found(),
            })
        );
        assert_eq!(
            value.found(),
            Found::Text(crate::fact::TextPreview {
                preview: "é".repeat(24),
                truncated: true,
            })
        );
    }

    #[test]
    fn strict_utf8_retains_subject_offsets_and_incomplete_sequences() {
        for (bytes, subject, length) in [
            (
                vec![b'a', 0xff],
                Utf8Subject::Input,
                Some(Count::from(1usize)),
            ),
            (vec![b'a', 0xc3], Utf8Subject::CapturedOutput, None),
        ] {
            let Err(Break::Error(error)) = decode_utf8_strict(bytes, "decode", subject.clone())
            else {
                panic!("invalid UTF-8")
            };
            let Reason::Invalid(failure) = error.reason else {
                panic!("invalid input")
            };
            assert_eq!(
                failure.cause,
                InvalidCause::Codec(CodecFailure::Utf8(CodecFailureUtf8 {
                    subject,
                    cause: Utf8Failure {
                        valid_up_to: Count::from(1usize),
                        error_len: length
                    },
                }))
            );
        }
    }
}
