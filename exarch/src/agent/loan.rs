//! The owner's state, lent only for a foreground shell call.

use atomic_refcell::{AtomicRefCell, AtomicRefMut};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

pub(crate) struct Loanable<T>(Option<T>);
impl<T> Loanable<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(Some(value))
    }

    pub(crate) fn lend<R>(&mut self, f: impl FnOnce(&Arc<Loan<T>>) -> R) -> R {
        struct Return<'a, T> {
            owner: &'a mut Option<T>,
            loan: Arc<Loan<T>>,
        }
        impl<T> Drop for Return<'_, T> {
            fn drop(&mut self) {
                *self.owner = self.loan.0.borrow_mut().take();
            }
        }
        let loan = Arc::new(Loan(AtomicRefCell::new(self.0.take())));
        let returned = Return {
            owner: &mut self.0,
            loan,
        };
        f(&returned.loan)
    }
}
impl<T> Deref for Loanable<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.0
            .as_ref()
            .expect("the owner cannot access a lent body")
    }
}
impl<T> DerefMut for Loanable<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.0
            .as_mut()
            .expect("the owner cannot access a lent body")
    }
}

pub(crate) struct Loan<T>(AtomicRefCell<Option<T>>);
impl<T> Loan<T> {
    pub(crate) fn get(&self) -> Option<AtomicRefMut<'_, T>> {
        AtomicRefMut::filter_map(self.0.borrow_mut(), Option::as_mut)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
