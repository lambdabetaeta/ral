//! Error constructors and the decoders of a builtin's arguments.

use super::{Break, Closure, Error, HandleInner, List, Map, Value};

pub fn sig(message: impl Into<String>) -> Break {
    Error::new(message).into()
}

pub(crate) fn sig_hint(message: impl Into<String>, hint: impl Into<String>) -> Break {
    Error::new(message).with_hint(hint).into()
}

/// Each decoder fails with a bare `Error`: a shape mismatch is never an exit,
/// and the capability decoder folds it straight into `PolicyError`.
impl Value {
    /// The one wording of a shape mismatch.
    fn expected(&self, ctx: &str, want: &str) -> Error {
        Error::new(format!("{ctx}: expected {want}, got {}", self.type_name()))
    }

    /// Borrow a `String` argument without copying it.
    ///
    /// # Errors
    /// `self` is not a `String`.
    pub fn as_str(&self, ctx: &str) -> Result<&str, Error> {
        match self {
            Self::String(s) => Ok(s.as_str()),
            _ => Err(self.expected(ctx, "String")),
        }
    }

    /// # Errors
    /// `self` is not `Bytes`.
    pub(crate) fn as_bytes(&self, ctx: &str) -> Result<&[u8], Error> {
        match self {
            Self::Bytes(b) => Ok(b),
            _ => Err(self.expected(ctx, "Bytes").with_hint(
                "a list of numbers is `ints-to-bytes`; a Bytes value comes from `from-bytes`",
            )),
        }
    }

    /// # Errors
    /// `self` is not a `Handle`.
    pub(crate) fn expect_handle(&self, ctx: &str) -> Result<&HandleInner, Error> {
        match self {
            Self::Handle(h) => Ok(h),
            _ => Err(self
                .expected(ctx, "Handle")
                .with_hint("use spawn to create a handle")),
        }
    }

    /// A spawn body takes no parameters: `comp.arrow()` is `None` for a
    /// block-shaped thunk.
    ///
    /// # Errors
    /// `self` is not a block.
    pub(crate) fn expect_thunk(&self, ctx: &str) -> Result<Closure, Error> {
        match self {
            Self::Thunk(c) if c.comp().arrow().is_none() => Ok(c.clone()),
            _ => Err(self
                .expected(ctx, "Block")
                .with_hint(format!("{ctx} requires a block: {ctx} {{ ... }}"))),
        }
    }

    /// # Errors
    /// `self` is not a `List`.
    pub fn as_list(&self, ctx: &str) -> Result<List, Error> {
        match self {
            Self::List(items) => Ok(items.clone()),
            _ => Err(self.expected(ctx, "List")),
        }
    }

    /// # Errors
    /// `self` is not a `Map`.
    pub(crate) fn as_map_ref(&self, ctx: &str) -> Result<&Map, Error> {
        match self {
            Self::Map(m) => Ok(m),
            _ => Err(self.expected(ctx, "Map")),
        }
    }

    /// Owning variant of [`Self::as_map_ref`].
    ///
    /// # Errors
    /// `self` is not a `Map`.
    pub fn as_map(&self, ctx: &str) -> Result<Map, Error> {
        self.as_map_ref(ctx).cloned()
    }
}

/// A contract file's settings: a map, or `()` for nothing to set.
pub fn settings_map(val: &Value) -> Option<std::borrow::Cow<'_, Map>> {
    match val {
        Value::Map(m) => Some(std::borrow::Cow::Borrowed(m)),
        Value::Unit => Some(std::borrow::Cow::Owned(Map::new())),
        _ => None,
    }
}

/// UTF-8 or a refusal naming `context` and offering `hint`.
///
/// # Errors
/// `bytes` is not valid UTF-8.
pub(crate) fn decode_utf8_strict(
    bytes: Vec<u8>,
    context: &str,
    hint: &str,
) -> Result<String, Break> {
    String::from_utf8(bytes).map_err(|e| sig_hint(format!("{context}: {e}"), hint))
}
