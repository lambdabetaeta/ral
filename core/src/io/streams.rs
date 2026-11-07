//! The pair every output-side datum comes in: one per stream, fd 1 and fd 2.

use serde::{Deserialize, Serialize};

/// One `T` for each of a computation's output streams.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Streams<T> {
    pub stdout: T,
    pub stderr: T,
}

impl<T> Streams<T> {
    /// Each stream's `T`, made by its own call of `make`.
    pub fn both(mut make: impl FnMut() -> T) -> Self {
        Self {
            stdout: make(),
            stderr: make(),
        }
    }

    pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> Streams<U> {
        Streams {
            stdout: f(self.stdout),
            stderr: f(self.stderr),
        }
    }

    pub const fn as_ref(&self) -> Streams<&T> {
        Streams {
            stdout: &self.stdout,
            stderr: &self.stderr,
        }
    }

    /// Each stream's `T` beside its name, stdout first.
    pub fn named(self) -> [(&'static str, T); 2] {
        [("stdout", self.stdout), ("stderr", self.stderr)]
    }
}

impl<A, B> Streams<(A, B)> {
    pub fn unzip(self) -> (Streams<A>, Streams<B>) {
        let Self {
            stdout: (out_a, out_b),
            stderr: (err_a, err_b),
        } = self;
        (
            Streams {
                stdout: out_a,
                stderr: err_a,
            },
            Streams {
                stdout: out_b,
                stderr: err_b,
            },
        )
    }
}
