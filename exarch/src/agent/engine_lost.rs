//! The one sentence every edge says about a severed engine.

use ral_core::carrier::Severed;
use ral_core::own::OwnDir;

/// A start failure is worth retrying; a mid-session death is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnginePhase {
    /// Before the session took a turn: the attach was refused, or the engine
    /// died before answering it.
    Starting,
    /// With a session under way, taking its whole state.
    Running,
}

/// A short fixed sentence for the window, plus the run directory to read.
///
/// The engine's own words and [`Severed::code`] go to the log through
/// [`Self::logged`].  An [`Error`](std::error::Error), so `Agent::root` can
/// carry it whole inside an [`io::Error`](std::io::Error) for a front-end to
/// downcast.
#[derive(Debug)]
pub struct EngineLost {
    phase: EnginePhase,
    cause: Severed,
    /// The run's directory, not the session's: a start failure has no session.
    log_dir: OwnDir,
}

impl EngineLost {
    pub fn starting(cause: &Severed, log_dir: &OwnDir) -> Self {
        Self::new(EnginePhase::Starting, cause, log_dir)
    }

    pub fn running(cause: &Severed, log_dir: &OwnDir) -> Self {
        Self::new(EnginePhase::Running, cause, log_dir)
    }

    fn new(phase: EnginePhase, cause: &Severed, log_dir: &OwnDir) -> Self {
        Self {
            phase,
            cause: cause.clone(),
            log_dir: log_dir.clone(),
        }
    }

    #[must_use]
    pub fn log_dir(&self) -> &OwnDir {
        &self.log_dir
    }

    #[must_use]
    pub const fn phase(&self) -> EnginePhase {
        self.phase
    }

    #[must_use]
    pub fn cause(&self) -> &Severed {
        &self.cause
    }

    /// The sentence, then the code and the engine's own account: for a log.
    #[must_use]
    pub fn logged(&self) -> String {
        format!("{self}\n\n({}) {}", self.cause.code(), self.cause)
    }
}

impl std::error::Error for EngineLost {}

/// No dash and no code: what happened, what to do, then the path.
impl std::fmt::Display for EngineLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sentence = match (self.phase, &self.cause) {
            // A refusal is deterministic: trying again meets it again.
            (EnginePhase::Starting, Severed::Refused(_)) => "The assistant could not be started.",
            (EnginePhase::Starting, _) => "The assistant could not be started. Try again.",
            (EnginePhase::Running, _) => {
                "The assistant stopped, so this conversation cannot go on. Start a new one."
            }
        };
        f.write_str(sentence)?;
        write!(f, " (details in {})", self.log_dir)
    }
}

#[cfg(test)]
mod tests;
