//! I/O redirects: the surface forms, and a list of them checked to bind each
//! stream at most once.

use serde::{Deserialize, Serialize};
use strum::{IntoStaticStr, VariantArray};

/// How a write redirect opens its file: `>` replaces it atomically, `>>`
/// appends, `>~` truncates and streams.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, IntoStaticStr, VariantArray,
)]
#[strum(serialize_all = "kebab-case")]
pub enum WriteMode {
    Write,
    Append,
    Stream,
}

crate::label!(typed WriteMode);

/// What `<` or `<<` feeds standard input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StdinSource<T> {
    /// `< path`.
    File(T),
    /// `<< str`: the payload itself.  One leading newline is dropped at
    /// evaluation, so a multiline body may start on the line below.
    Here(T),
}

/// An I/O redirect onto one of ral's three streams.
///
/// Its operand `T` is the parsed word, then the elaborated value, then the
/// evaluated string.  A field of [`Ast::Call`] and [`Ast::Scope`] rather than
/// an argument, so it can never pass for a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Redirect<T> {
    Stdin(StdinSource<T>),
    Stdout(WriteMode, T),
    Stderr(WriteMode, T),
    /// `2>&1`.
    StderrToStdout,
}

/// Where standard error goes once its redirect is bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StderrTarget<T> {
    File(WriteMode, T),
    /// `2>&1`: wherever standard output goes, position-independently.
    Stdout,
}

/// A redirect list, checked: each stream is bound at most once, so there is
/// one final destination per stream and nothing is opened only to be
/// overridden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redirects<T> {
    pub stdin: Option<StdinSource<T>>,
    pub stdout: Option<(WriteMode, T)>,
    pub stderr: Option<StderrTarget<T>>,
}

impl<T> Default for Redirects<T> {
    fn default() -> Self {
        Self {
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }
}

impl<T> Redirects<T> {
    /// Binds `r` to its stream; `Err` says why a second binding is refused.
    pub(crate) fn bind(&mut self, r: Redirect<T>) -> Result<(), &'static str> {
        match r {
            Redirect::Stdin(src) if self.stdin.is_none() => self.stdin = Some(src),
            Redirect::Stdin(_) => {
                return Err("standard input is fed twice; which one do you mean? \
                            A command reads from one source.");
            }
            Redirect::Stdout(mode, t) if self.stdout.is_none() => self.stdout = Some((mode, t)),
            Redirect::Stdout(..) => {
                return Err(
                    "standard output is redirected twice; which one do you mean? \
                            To write to both files, pipe through `tee`.",
                );
            }
            Redirect::Stderr(mode, t) => match self.stderr {
                None => self.stderr = Some(StderrTarget::File(mode, t)),
                Some(StderrTarget::File(..)) => {
                    return Err("standard error is redirected twice; which one do you mean?");
                }
                Some(StderrTarget::Stdout) => {
                    return Err(
                        "standard error is redirected twice: `2>&1` already sends it \
                                with standard output; which one do you mean?",
                    );
                }
            },
            Redirect::StderrToStdout => match self.stderr {
                None => self.stderr = Some(StderrTarget::Stdout),
                Some(StderrTarget::File(..)) => {
                    return Err("standard error is redirected twice: `2>&1` would override \
                                the `2>` before it; which one do you mean?");
                }
                Some(StderrTarget::Stdout) => {
                    return Err("`2>&1` is written twice; write it once");
                }
            },
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.stdin.is_none() && self.stdout.is_none() && self.stderr.is_none()
    }

    /// The operands in opening order: stdin, stdout, stderr.
    pub(crate) fn operands(&self) -> impl Iterator<Item = &T> {
        let stdin = self
            .stdin
            .iter()
            .map(|(StdinSource::File(t) | StdinSource::Here(t))| t);
        let stdout = self.stdout.iter().map(|(_, t)| t);
        let stderr = self.stderr.iter().filter_map(|e| match e {
            StderrTarget::File(_, t) => Some(t),
            StderrTarget::Stdout => None,
        });
        stdin.chain(stdout).chain(stderr)
    }

    pub(crate) fn try_map<U, E>(
        &self,
        mut f: impl FnMut(&T) -> Result<U, E>,
    ) -> Result<Redirects<U>, E> {
        Ok(Redirects {
            stdin: match &self.stdin {
                Some(StdinSource::File(t)) => Some(StdinSource::File(f(t)?)),
                Some(StdinSource::Here(t)) => Some(StdinSource::Here(f(t)?)),
                None => None,
            },
            stdout: match &self.stdout {
                Some((mode, t)) => Some((*mode, f(t)?)),
                None => None,
            },
            stderr: match &self.stderr {
                Some(StderrTarget::File(mode, t)) => Some(StderrTarget::File(*mode, f(t)?)),
                Some(StderrTarget::Stdout) => Some(StderrTarget::Stdout),
                None => None,
            },
        })
    }

    pub(crate) fn map<U>(&self, mut f: impl FnMut(&T) -> U) -> Redirects<U> {
        let Ok(r) = self.try_map(|t| Ok::<_, std::convert::Infallible>(f(t)));
        r
    }
}
