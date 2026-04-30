//! Path resolution for grant matching.
//!
//! Every grant-touching path obeys one operational rule, in this
//! order, and each premise has its own sibling file:
//!
//! ```text
//!   expand σ p   ⇓  q     stage 1: ~ and xdg: at the head        (sigil)
//!   lex   σ q    ⇓  r     stage 2: cwd-anchor + ./.. normalise   (lex)
//!   canon r      ⇓  c     stage 3: realpath, ancestor-walk fallback (canon)
//!   match a c P           stage 4: alias-aware containment       (lex::path_within)
//! ```
//!
//! [`tilde`] holds the syntactic shape consumed by stage 1 (and
//! by the lexer); [`home`] resolves `$HOME` once for the whole
//! pipeline; [`which`] is a sibling for `PATH` search.
//!
//! Most call sites want the most-used names without reaching
//! into a child module — those are re-exported below.  The full
//! API lives in the children, named by stage.

pub mod canon;
pub mod home;
pub mod lex;
pub mod resolver;
pub mod sigil;
pub mod tilde;
pub mod which;

pub use lex::{path_aliases, path_within, resolve_path};
pub use resolver::{CanonMode, Resolver};
pub use which::{locate, resolve_in_path};
