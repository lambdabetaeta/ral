//! The mail and the obligations derived from it.

use super::{
    AgentId, ChildStatus, Control, JobId, NonEmpty, Post, PostId, Recorded, Started, Voice,
};
use std::collections::BTreeMap;

#[derive(Debug, PartialEq)]
pub struct Mail {
    parent: Option<AgentId>,
    returns: bool,
    pending: BTreeMap<PostId, Post>,
    /// A post by the parent was taken up since the last `Status`.
    answering: bool,
    status: Option<ChildStatus>,
    job: Option<JobId>,
    /// The commit that last named a post or job; a commit names at most one, ids only grow.
    named: u64,
}

impl Mail {
    pub(super) fn new(started: &Started) -> Self {
        Self {
            parent: started.parent,
            returns: started.returns,
            pending: BTreeMap::new(),
            answering: false,
            status: None,
            job: None,
            named: 0,
        }
    }

    pub fn parent(&self) -> Option<AgentId> {
        self.parent
    }
    pub fn returns(&self) -> bool {
        self.returns
    }
    pub fn pending(&self) -> impl Iterator<Item = (PostId, &Post)> {
        self.pending.iter().map(|(&id, post)| (id, post))
    }
    pub fn post(&self, id: PostId) -> Option<&Post> {
        self.pending.get(&id)
    }
    pub fn by_parent(&self, post: &Post) -> bool {
        match self.parent {
            Some(parent) => post.from == Some(parent),
            None => post.voice == Some(Voice::Human) && post.from.is_none(),
        }
    }
    pub fn owes(&self) -> bool {
        self.returns && (self.answering || self.pending.values().any(|post| self.by_parent(post)))
    }
    pub fn status(&self) -> Option<&ChildStatus> {
        self.status.as_ref()
    }
    pub fn job(&self) -> Option<JobId> {
        self.job
    }

    pub(super) fn step(&mut self, record: &Recorded<&Control>) -> Result<(), String> {
        let commit = record.locus().seq().commit();
        match record.value() {
            Control::SessionResumed { returns, .. } => {
                self.returns = *returns;
                self.answering &= *returns;
            }
            Control::JobOpened => {
                if self.job.is_some() || commit <= self.named {
                    return Err("JobOpened needs no open job and a fresh commit number".into());
                }
                self.job = Some(JobId::from(commit));
                self.named = commit;
            }
            Control::JobSettled { job, .. } => {
                if self.job != Some(*job) {
                    return Err(format!(
                        "JobSettled names {job:?}, but the open job is {:?}",
                        self.job
                    ));
                }
                self.job = None;
            }
            Control::Status { status } => {
                self.status = Some(status.clone());
                self.answering = false;
            }
            Control::Posted(post) => {
                if commit <= self.named {
                    return Err("Posted needs a fresh commit number: a commit names at most one post or job".into());
                }
                drop(self.pending.insert(PostId(commit), post.clone()));
                self.named = commit;
            }
            Control::Taken { posts } => self.remove(posts, true)?,
            Control::Withdrawn { posts } => self.remove(posts, false)?,
            Control::SessionStarted(_)
            | Control::Scheduled { .. }
            | Control::Unscheduled { .. }
            | Control::ModelChanged { .. }
            | Control::WindowNarrowed { .. } => {}
        }
        Ok(())
    }

    fn remove(&mut self, posts: &NonEmpty<PostId>, taken: bool) -> Result<(), String> {
        for id in posts {
            let post = self
                .pending
                .remove(id)
                .ok_or_else(|| format!("post {id:?} is not pending"))?;
            self.answering |= taken && self.returns && self.by_parent(&post);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Selected;

    #[test]
    fn birth_owes_nothing_for_every_parent_and_return_policy() {
        for parent in [None, Some(AgentId::new(7))] {
            for returns in [false, true] {
                let started = Started {
                    session_id: AgentId::new(8),
                    name: "newborn".into(),
                    parent,
                    returns,
                    selected: Selected::for_test("test-model"),
                    log_dir: std::path::PathBuf::default(),
                    at_unix_ms: 0,
                };
                assert!(!Mail::new(&started).owes(), "{started:?}");
            }
        }
    }
}
