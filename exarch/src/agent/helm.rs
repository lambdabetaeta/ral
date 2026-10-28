use super::Agent;
use std::fmt;
use std::sync::Weak;

/// A host's weak hold; never keeps the agent past its avatar.
/// Once ended, steering is refused and other operations are empty or no-ops.
#[derive(Clone)]
pub struct Helm(pub(super) Weak<Agent>);

/// The agent behind a [`Helm`] has ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ended;

impl fmt::Display for Ended {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the agent has ended")
    }
}

impl std::error::Error for Ended {}

impl Helm {
    /// Queue a human message for the next tool boundary or exchange.
    ///
    /// # Errors
    /// Refuses a message once the agent has ended.
    pub fn steer(&self, text: String) -> Result<(), Ended> {
        let agent = self
            .0
            .upgrade()
            .filter(|agent| !agent.token.terminated())
            .ok_or(Ended)?;
        agent
            .mailbox
            .steer_live(text, &agent.token)
            .then_some(())
            .ok_or(Ended)
    }

    /// Abandon the exchange and return its queued prompts; empty once ended.
    pub fn stop(&self) -> Vec<String> {
        self.0
            .upgrade()
            .filter(|agent| !agent.token.terminated())
            .map_or_else(Vec::new, |agent| agent.stop())
    }

    /// End this agent and its subtree for good.
    pub fn end(&self) {
        if let Some(agent) = self.0.upgrade() {
            agent.end();
        }
    }

    /// Take every queued prompt back, oldest first.
    pub fn recall(&self) -> Vec<String> {
        self.0
            .upgrade()
            .filter(|agent| !agent.token.terminated())
            .map_or_else(Vec::new, |agent| agent.recall())
    }

    /// The prompts and commands still waiting, oldest first.
    pub fn queued(&self) -> Vec<String> {
        self.0
            .upgrade()
            .filter(|agent| !agent.token.terminated())
            .map_or_else(Vec::new, |agent| agent.queued())
    }
}
