//! String value (the inner of `Value::String`).

use std::borrow::Cow;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// An immutable string, shared on clone.
///
/// `Arc<String>`, not `Arc<str>`: an owned `String` moves in without a copy,
/// and, while the count is one, back out.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Str(Arc<String>);

impl Str {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Free while this is the only holder.
    pub fn into_string(self) -> String {
        Arc::unwrap_or_clone(self.0)
    }
}

impl Deref for Str {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for Str {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for Str {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl fmt::Display for Str {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

impl fmt::Debug for Str {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl From<String> for Str {
    fn from(s: String) -> Self {
        Self(Arc::new(s))
    }
}

impl From<&str> for Str {
    fn from(s: &str) -> Self {
        Self::from(s.to_owned())
    }
}

impl From<Cow<'_, str>> for Str {
    fn from(s: Cow<'_, str>) -> Self {
        Self::from(s.into_owned())
    }
}
