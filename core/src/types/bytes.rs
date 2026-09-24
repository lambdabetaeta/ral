//! Bytes value (the inner of `Value::Bytes`).

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

/// An immutable byte string, shared on clone.
///
/// `Arc<Vec<u8>>`, not `Arc<[u8]>`: an owned buffer moves in without a copy,
/// and, while the count is one, back out.
#[derive(Clone, PartialEq, Eq)]
pub struct Bytes(Arc<Vec<u8>>);

impl Bytes {
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Free while this is the only holder.
    pub fn into_vec(self) -> Vec<u8> {
        Arc::unwrap_or_clone(self.0)
    }
}

impl Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_slice(), f)
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(b: Vec<u8>) -> Self {
        Self(Arc::new(b))
    }
}

impl From<&[u8]> for Bytes {
    fn from(b: &[u8]) -> Self {
        Self::from(b.to_vec())
    }
}
