//! The baseline walk starts alongside boot and settles before the first guest act.

use crate::folder::manifest::{Manifest, Progress, Stop};

pub(crate) struct Baseline {
    walk: Walk,
    stop: Stop,
}

enum Walk {
    Running(std::thread::JoinHandle<Result<Manifest, String>>),
    Settled(Result<Manifest, String>),
}

impl Baseline {
    pub(crate) fn spawn(
        root: std::path::PathBuf,
        stop: &Stop,
        mut progress: Box<dyn FnMut(u64) + Send>,
    ) -> Self {
        let walk_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            Manifest::of_folder_via(&root, &walk_stop, &mut progress as Progress<'_>)
        });
        Self {
            walk: Walk::Running(handle),
            stop: stop.clone(),
        }
    }

    pub(crate) fn manifest(&mut self) -> Result<&Manifest, String> {
        match self.walk {
            Walk::Settled(ref result) => result.as_ref().map_err(Clone::clone),
            Walk::Running(_) => {
                self.settle();
                self.manifest()
            }
        }
    }

    fn settle(&mut self) {
        let walk = std::mem::replace(&mut self.walk, Walk::Settled(Err(String::new())));
        self.walk = Walk::Settled(match walk {
            Walk::Settled(result) => result,
            Walk::Running(walk) => match walk.join() {
                Ok(result) => result,
                Err(_) => Err(
                    "Synod crashed while reading the folder, so it cannot say what changed."
                        .to_string(),
                ),
            },
        });
    }
}

impl Drop for Baseline {
    fn drop(&mut self) {
        // Settling takes the walk, so only an abandoned walk is stopped.
        if matches!(self.walk, Walk::Running(_)) {
            self.stop.stop();
            self.settle();
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::test_fixture::workshop;

    /// A stopped walk is an error; joining must not trip its stop switch.
    #[test]
    fn a_settled_baseline_hands_back_the_whole_folder() {
        let dir = workshop("baseline-settled");
        std::fs::write(dir.path().join("letter.txt"), b"dear all").expect("fixture");

        let mut baseline = Baseline::spawn(
            dir.path().to_path_buf(),
            &Stop::default(),
            Box::new(|_so_far| {}),
        );
        let manifest = baseline.manifest().expect("the baseline walk succeeds");
        assert!(manifest.entries.contains_key("letter.txt"));

        assert!(
            baseline
                .manifest()
                .expect("a settled baseline stays settled")
                .entries
                .contains_key("letter.txt")
        );
    }

    #[test]
    fn an_abandoned_baseline_stops_its_own_walk() {
        let dir = workshop("baseline-abandoned");
        std::fs::write(dir.path().join("letter.txt"), b"dear all").expect("fixture");

        let baseline = Baseline::spawn(
            dir.path().to_path_buf(),
            &Stop::default(),
            Box::new(|_so_far| {}),
        );
        drop(baseline);
    }
}
