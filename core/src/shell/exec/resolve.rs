//! What a command head names.
//!
//! A bare name is looked up in the env, then the handler stack, then `PATH`;
//! `^name` and a path are always externals.

use super::command::{Command, Request};
use crate::ir::{CommandName, CommandWord, Env, ValClos};
use crate::shell::runtime::{HandlerLookup, Shell, lookup};

/// What a command head names.
pub(crate) enum Resolution {
    Env(ValClos),
    Handler(HandlerLookup),
    /// A program, not yet looked up.
    External(Command<Request>),
}

impl Resolution {
    /// Resolve `word`. `env` is the lexical env, not `shell.env`.
    pub(crate) fn of(word: &CommandWord, env: &Env, shell: &Shell) -> Self {
        let external = |name| Self::External(Command::from_name(name, &shell.context));
        let CommandWord::Name(name @ CommandName::Bare(bare)) = word else {
            return external(word.name());
        };
        if let Some(value) = lookup(bare, env, &shell.sig) {
            return Self::Env(value.clone());
        }
        shell
            .lookup_handler(bare)
            .map_or_else(|| external(name), Self::Handler)
    }
}
