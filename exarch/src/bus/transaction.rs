//! Cell entry and its poison-preserving wait site.

use crate::record::AgentId;
use std::sync::Arc;

#[derive(Clone, Copy)]
pub(super) enum Silent {
    /// The cell restates the log: a wait must keep poison, not recover it.
    CellWait,
}
impl crate::site::Site for Silent {}

#[cfg(debug_assertions)]
std::thread_local! {
    static HELD: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub(super) struct Entered {
    #[cfg(debug_assertions)]
    cell: usize,
}

impl Entered {
    pub(super) fn new<T>(cell: &Arc<T>, id: AgentId) -> Self {
        #[cfg(debug_assertions)]
        {
            let cell = Arc::as_ptr(cell) as usize;
            HELD.with_borrow_mut(|held| {
                assert!(!held.contains(&cell), "re-entered agent {id}'s transaction");
                held.push(cell);
            });
            Self { cell }
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = (cell, id);
            Self {}
        }
    }
}

impl Drop for Entered {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        HELD.with_borrow_mut(|held| {
            let cell = held.pop();
            debug_assert_eq!(cell, Some(self.cell));
        });
    }
}
