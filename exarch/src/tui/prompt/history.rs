use ral_core::own::OwnDir;
use ral_core::path::{Visibility, XdgKind};

const FILE: &str = "prompt-history.json";
const MAX_ENTRIES: usize = 500;
const MAX_BYTES: usize = 1024 * 1024;

/// Submitted prompts, oldest first, with the store that shares them across
/// sessions and the entry an arrow browse is showing.  The default keeps no
/// store.
#[derive(Default)]
pub(in crate::tui) struct History {
    entries: Vec<String>,
    store: Option<Persistence>,
    /// Indexes `entries`; every replacement of `entries` resets it.
    browse: Option<usize>,
}

impl History {
    /// The user's shared history.
    ///
    /// # Errors
    /// If the store cannot be opened or read; an unreadable file is never
    /// overwritten.
    pub(in crate::tui) fn open() -> Result<Self, String> {
        OwnDir::xdg("exarch", XdgKind::State)
            .map_err(|e| format!("Cannot open prompt history: {e}"))
            .and_then(|dir| Self::stored(Persistence(dir)))
    }

    #[cfg(test)]
    pub(super) fn at(dir: OwnDir) -> Result<Self, String> {
        Self::stored(Persistence(dir))
    }

    fn stored(store: Persistence) -> Result<Self, String> {
        Ok(Self {
            entries: store.load()?,
            store: Some(store),
            browse: None,
        })
    }

    pub(super) fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Append `prompt` and end any browse.  The entry is kept even when the
    /// store refuses it.
    pub(super) fn record(&mut self, prompt: &str) -> Result<(), String> {
        self.browse = None;
        if self.entries.last().is_none_or(|entry| entry != prompt) {
            self.entries.push(prompt.to_owned());
            bound(&mut self.entries);
        }
        if let Some(store) = &self.store {
            self.entries = store.record(prompt)?;
        }
        Ok(())
    }

    pub(super) fn is_browsing(&self) -> bool {
        self.browse.is_some()
    }

    pub(super) fn stop_browsing(&mut self) {
        self.browse = None;
    }

    /// Whether `draft` is what browsing last showed: its entry, or the empty
    /// draft it started from.
    pub(super) fn shows(&self, draft: &str) -> bool {
        self.browse
            .map_or(draft.is_empty(), |i| self.entries[i] == draft)
    }

    /// Step to the next older entry; `None`, unmoved, at the oldest.
    pub(super) fn older(&mut self) -> Option<String> {
        let i = self.browse.unwrap_or(self.entries.len()).checked_sub(1)?;
        self.browse = Some(i);
        Some(self.entries[i].clone())
    }

    /// Step to the next newer entry, or past the newest to the empty draft,
    /// ending the browse; `None` when not browsing.
    pub(super) fn newer(&mut self) -> Option<String> {
        let i = self.browse? + 1;
        self.browse = (i < self.entries.len()).then_some(i);
        Some(
            self.browse
                .map_or_else(String::new, |i| self.entries[i].clone()),
        )
    }
}

struct Persistence(OwnDir);

impl Persistence {
    fn load(&self) -> Result<Vec<String>, String> {
        let file = match self.0.open(FILE) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("Cannot read prompt history: {e}")),
        };
        let bytes = ral_core::io::read_bounded(file, MAX_BYTES * 8)
            .map_err(|e| format!("Cannot read prompt history: {e}"))?
            .ok_or_else(|| format!("Prompt history is too large; move {FILE} aside to reset it"))?;
        let mut entries = serde_json::from_slice::<Vec<String>>(&bytes)
            .map_err(|e| format!("Cannot decode prompt history in {FILE}: {e}"))?;
        bound(&mut entries);
        Ok(entries)
    }

    fn record(&self, prompt: &str) -> Result<Vec<String>, String> {
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

fn bound(entries: &mut Vec<String>) {
    entries.retain(|entry| !entry.is_empty() && entry.len() <= MAX_BYTES);
    let keep = entries
        .iter()
        .rev()
        .take(MAX_ENTRIES)
        .scan(0, |bytes, entry| {
            *bytes += entry.len();
            (*bytes <= MAX_BYTES).then_some(())
        })
        .count();
    entries.drain(..entries.len() - keep);
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    fn history(entries: &[&str]) -> History {
        let mut history = History::default();
        for entry in entries {
            history.record(entry).unwrap();
        }
        history
    }

    #[test]
    fn browsing_walks_back_stops_at_the_oldest_and_returns_to_the_empty_draft() {
        let mut h = history(&["one", "two"]);
        assert!(h.shows("") && !h.shows("two"));
        assert_eq!(h.newer(), None, "not browsing");
        assert_eq!(h.older().as_deref(), Some("two"));
        assert_eq!(h.older().as_deref(), Some("one"));
        assert_eq!(h.older(), None, "the oldest stays put");
        assert!(h.shows("one") && !h.shows(""));
        assert_eq!(h.newer().as_deref(), Some("two"));
        assert_eq!(h.newer().as_deref(), Some(""));
        assert!(!h.is_browsing());
        assert_eq!(History::default().older(), None);
    }

    #[test]
    fn recording_ends_the_browse_and_skips_an_adjacent_duplicate() {
        let mut h = history(&["one"]);
        h.older();
        h.record("one").unwrap();
        assert!(!h.is_browsing());
        assert_eq!(h.entries(), ["one"]);
        h.older();
        h.stop_browsing();
        assert!(!h.is_browsing());
    }

    #[test]
    fn an_opened_history_merges_and_a_refused_save_keeps_the_entry() {
        let dir = OwnDir::for_test("prompt-history-open").unwrap();
        History::at(OwnDir::clone(&dir))
            .unwrap()
            .record("other session")
            .unwrap();
        let mut h = History::at(OwnDir::clone(&dir)).unwrap();
        assert_eq!(h.entries(), ["other session"]);
        dir.open_write(FILE).unwrap().set_len(0).unwrap();
        assert!(h.record("kept").unwrap_err().contains("prompt history"));
        assert_eq!(h.entries(), ["other session", "kept"]);
    }

    #[test]
    fn persistence_round_trip_is_private_and_bounded() {
        let dir = OwnDir::for_test("prompt-history").unwrap();
        let persistence = Persistence(OwnDir::clone(&dir));
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
        let first = Persistence(OwnDir::clone(&dir));
        let second = Persistence(OwnDir::clone(&dir));
        first.record("one").unwrap();
        second.record("two").unwrap();
        assert_eq!(first.load().unwrap(), ["one", "two"]);
    }

    #[test]
    fn corrupt_history_is_reported() {
        let dir = OwnDir::for_test("prompt-history-corrupt").unwrap();
        dir.store(FILE, &42, Visibility::Secret).unwrap();
        assert!(History::at(OwnDir::clone(&dir)).is_err());
    }

    #[test]
    fn lock_contention_is_reported_and_can_be_retried() {
        let dir = OwnDir::for_test("prompt-history-locked").unwrap();
        let persistence = Persistence(OwnDir::clone(&dir));
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
        let persistence = Persistence(OwnDir::clone(&dir));
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
