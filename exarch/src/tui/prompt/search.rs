// Interaction inspired by OpenAI Codex's history_search.rs (Apache-2.0).
// Independently implemented: the editor remains untouched until acceptance.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub(super) struct Search {
    pub(super) query: String,
    pub(super) matched: Option<usize>,
}

impl Search {
    pub(super) fn restart(&mut self, history: &[String]) {
        self.matched = None;
        self.older(history);
    }

    pub(super) fn backspace(&mut self) {
        if let Some((start, _)) = self.query.grapheme_indices(true).next_back() {
            self.query.truncate(start);
        }
    }

    pub(super) fn older(&mut self, history: &[String]) {
        let end = self.matched.unwrap_or(history.len());
        let previous = self.matched.map(|i| &history[i]);
        if let Some(i) = (0..end)
            .rev()
            .find(|&i| history[i].contains(&self.query) && previous != Some(&history[i]))
        {
            self.matched = Some(i);
        }
    }

    pub(super) fn newer(&mut self, history: &[String]) {
        let Some(current) = self.matched else { return };
        if let Some(i) = (current + 1..history.len())
            .find(|&i| history[i].contains(&self.query) && history[i] != history[current])
        {
            self.matched = Some(i);
        }
    }

    pub(super) fn status(&self, history: &[String]) -> String {
        let result = match self.matched {
            Some(i) => display(&history[i], 100),
            None if self.query.is_empty() => "type to search".into(),
            None => "no match".into(),
        };
        format!("reverse search [{}]: {result}", display(&self.query, 60))
    }
}

fn display(text: &str, max: usize) -> String {
    let normalized: String = text
        .chars()
        .map(|c| match c {
            '\n' => '↵',
            '\t' => '⇥',
            c if c.is_control() => '�',
            c => c,
        })
        .collect();
    if normalized.width() <= max {
        return normalized;
    }
    let mut shown = String::new();
    let mut cells = 0;
    for grapheme in normalized.graphemes(true) {
        let width = grapheme.width();
        if cells + width > max.saturating_sub(1) {
            break;
        }
        cells += width;
        shown.push_str(grapheme);
    }
    if max > 0 {
        shown.push('…');
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_budgets_cells_without_splitting_graphemes() {
        for budget in 0..10 {
            let shown = display("界e\u{301}👨‍👩‍👧‍👦tail", budget);
            assert!(shown.width() <= budget);
        }
        assert_eq!(display("界界", 3), "界…");
        assert_eq!(display("e\u{301}tail", 2), "e\u{301}…");
        assert_eq!(display("\n\t\u{1b}", 3), "↵⇥�");
    }
}
