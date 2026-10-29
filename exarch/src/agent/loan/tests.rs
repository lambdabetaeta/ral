use super::*;

#[test]
fn a_loan_returns_mutations_and_revokes_every_retained_address() {
    let mut owner = Loanable::new(vec![1]);
    let retained = owner.lend(|loan| {
        loan.get().expect("the dispatch owns the loan").push(2);
        loan.clone()
    });
    assert_eq!(*owner, [1, 2]);
    assert!(retained.get().is_none(), "dispatch returned the value");
    owner.push(3);
    assert_eq!(*owner, [1, 2, 3]);
}

#[test]
fn an_unwinding_dispatch_restores_the_mutated_value() {
    let mut owner = Loanable::new(vec![1]);
    let mut retained = None;
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        owner.lend(|loan| {
            retained = Some(loan.clone());
            loan.get().expect("the dispatch owns the loan").push(2);
            panic!("dispatch panicked");
        });
    }));
    assert!(panic.is_err());
    assert_eq!(*owner, [1, 2]);
    assert!(retained.unwrap().get().is_none());
    owner.lend(|loan| loan.get().unwrap().push(3));
    assert_eq!(*owner, [1, 2, 3]);
}
