use std::io::Read;

use ral_core::own::OwnDir;
use ral_core::path::Visibility;
#[cfg(not(test))]
use ral_core::path::XdgKind;

const FILE: &str = "prompt-history.json";
const MAX_ENTRIES: usize = 500;
const MAX_BYTES: usize = 1024 * 1024;

pub(super) struct Persistence(OwnDir);

impl Persistence {
    #[cfg(not(test))]
    pub(super) fn open() -> Result<Self, String> {
        OwnDir::xdg("exarch", XdgKind::State)
            .map(Self)
            .map_err(|e| format!("Cannot open prompt history: {e}"))
    }

    #[cfg(test)]
    pub(super) fn at(dir: OwnDir) -> Self {
        Self(dir)
    }

    pub(super) fn load(&self) -> Result<Vec<String>, String> {
        let file = match self.0.open(FILE) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("Cannot read prompt history: {e}")),
        };
        let limit = MAX_BYTES * 8;
        let mut bytes = Vec::new();
        file.take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("Cannot read prompt history: {e}"))?;
        if bytes.len() > limit {
            return Err(
                "Prompt history is too large; move prompt-history.json aside to reset it".into(),
            );
        }
        let mut entries = serde_json::from_slice::<Vec<String>>(&bytes)
            .map_err(|e| format!("Cannot decode prompt history in {FILE}: {e}"))?;
        bound(&mut entries);
        Ok(entries)
    }

    pub(super) fn record(&self, prompt: &str) -> Result<Vec<String>, String> {
        let lock = self
            .0
            .open_write("prompt-history.lock")
            .map_err(|e| format!("Cannot open prompt history lock: {e}"))?;
        lock.try_lock().map_err(|e| {
            format!("Cannot lock prompt history; this submission was not saved: {e}")
        })?;
        let mut entries = self.load()?;
        if entries.last().is_none_or(|entry| entry != prompt) {
            entries.push(prompt.to_owned());
            bound(&mut entries);
        }
        self.save(&entries)?;
        Ok(entries)
    }

    fn save(&self, entries: &[String]) -> Result<(), String> {
        self.0
            .store(FILE, &entries, Visibility::Secret)
            .map_err(|e| format!("Cannot save prompt history: {e}"))
    }
}

pub(super) fn bound(entries: &mut Vec<String>) {
    entries.retain(|entry| !entry.is_empty() && entry.len() <= MAX_BYTES);
    let mut bytes = 0;
    let keep = entries
        .iter()
        .rev()
        .take(MAX_ENTRIES)
        .take_while(|entry| {
            bytes += entry.len();
            bytes <= MAX_BYTES
        })
        .count();
    entries.drain(..entries.len() - keep);
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn persistence_round_trip_is_private_and_bounded() {
        let dir = OwnDir::for_test("prompt-history").unwrap();
        let persistence = Persistence::at(OwnDir::clone(&dir));
        let mut entries: Vec<_> = (0..600).map(|i| format!("prompt {i}\n界")).collect();
        bound(&mut entries);
        persistence.save(&entries).unwrap();
        assert_eq!(persistence.load().unwrap(), entries);
        assert_eq!(entries.len(), MAX_ENTRIES);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.path().join(FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn separate_sessions_merge_history_under_the_lock() {
        let dir = OwnDir::for_test("prompt-history-shared").unwrap();
        let first = Persistence::at(OwnDir::clone(&dir));
        let second = Persistence::at(OwnDir::clone(&dir));
        first.record("one").unwrap();
        second.record("two").unwrap();
        assert_eq!(first.load().unwrap(), ["one", "two"]);
    }

    #[test]
    fn corrupt_history_is_reported() {
        let dir = OwnDir::for_test("prompt-history-corrupt").unwrap();
        dir.store(FILE, &42, Visibility::Secret).unwrap();
        assert!(Persistence::at(OwnDir::clone(&dir)).load().is_err());
    }

    #[test]
    fn lock_contention_is_reported_and_can_be_retried() {
        let dir = OwnDir::for_test("prompt-history-locked").unwrap();
        let persistence = Persistence::at(OwnDir::clone(&dir));
        let lock = dir.open_write("prompt-history.lock").unwrap();
        lock.try_lock().unwrap();
        assert!(
            persistence
                .record("retry me")
                .unwrap_err()
                .contains("not saved")
        );
        drop(lock);
        assert_eq!(persistence.record("retry me").unwrap(), ["retry me"]);
    }

    #[test]
    fn oversized_history_is_rejected_before_deserializing() {
        let dir = OwnDir::for_test("prompt-history-oversized").unwrap();
        dir.open_write(FILE)
            .unwrap()
            .set_len((MAX_BYTES * 8 + 1) as u64)
            .unwrap();
        let persistence = Persistence::at(OwnDir::clone(&dir));
        assert!(persistence.load().unwrap_err().contains("too large"));
    }

    #[test]
    fn byte_budget_keeps_a_recent_suffix() {
        let mut entries = vec![
            "a".repeat(MAX_BYTES / 2),
            "b".repeat(MAX_BYTES / 2),
            "new".into(),
        ];
        bound(&mut entries);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1], "new");
    }
}
