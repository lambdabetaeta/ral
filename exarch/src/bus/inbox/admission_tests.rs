use super::*;
use ral_core::process::CancelCause;
use std::sync::mpsc;

#[test]
fn a_pre_stop_draw_cannot_reset_stop_or_remove_new_nudges() {
    let inbox = Inbox::new();
    let mailbox = inbox.mailbox();
    let token = cancel::Token::new();
    inbox.push_user("old".into());
    let drawn = inbox.draw(|| ParkMode::Quiesce, &token).unwrap();
    let (hold, _, ()) = mailbox.begin_abandon(Some(&token), || ()).unwrap();
    inbox.push(Post::Nudge {
        prompt: 2,
        text: "new".into(),
    });
    assert!(!drawn.stamp.reset(&token));
    drop(hold);
    assert!(!drawn.stamp.reset(&token));
    assert!(token.is_cancelled());
    assert!(matches!(
        inbox.next_item(),
        Some(Next::Item(Item::Nudge { .. }))
    ));
}

#[test]
fn an_accepted_arrival_cannot_reset_a_later_stop() {
    let inbox = Inbox::new();
    let token = cancel::Token::new();
    inbox.push_user("old".into());
    let drawn = inbox.draw(|| ParkMode::Quiesce, &token).unwrap();
    let (accepted, acceptance) = mpsc::channel();
    let (resume, resumed) = mpsc::channel();
    let worker_token = token.clone();
    let worker = std::thread::spawn(move || {
        assert!(drawn.stamp.current(&worker_token));
        accepted.send(()).unwrap();
        resumed.recv().unwrap();
        assert!(!drawn.stamp.reset(&worker_token));
    });
    acceptance.recv().unwrap();
    let (hold, _, ()) = inbox.mailbox().begin_abandon(Some(&token), || ()).unwrap();
    drop(hold);
    resume.send(()).unwrap();
    worker.join().unwrap();
    assert!(token.is_cancelled());
}

#[test]
fn overlapping_delivery_holds_block_new_uptake_until_the_last_release() {
    let inbox = Inbox::new();
    let mailbox = inbox.mailbox();
    let token = cancel::Token::new();
    let (first, _, ()) = mailbox.begin_abandon(Some(&token), || ()).unwrap();
    let (second, _, ()) = mailbox.begin_abandon(Some(&token), || ()).unwrap();
    assert!(mailbox.steer_live("next".into(), &token));
    let (parked, observed) = mpsc::channel();
    let (done, result) = mpsc::channel();
    let worker_inbox = inbox.clone();
    let worker_token = token.clone();
    let worker = std::thread::spawn(move || {
        let drawn = worker_inbox
            .draw(
                || {
                    parked.send(()).unwrap();
                    ParkMode::Held
                },
                &worker_token,
            )
            .unwrap();
        assert!(drawn.stamp.reset(&worker_token));
        done.send(drawn.next).unwrap();
    });
    observed.recv().unwrap();
    assert!(result.try_recv().is_err());
    drop(first);
    assert_eq!(inbox.shared.lock().stopping, 1);
    assert!(result.try_recv().is_err());
    drop(second);
    assert!(matches!(result.recv().unwrap(), Next::Item(Item::Human(text)) if text == "next"));
    worker.join().unwrap();
    assert!(!token.is_cancelled());
}

#[test]
fn detached_mid_exchange_items_retain_their_generation() {
    let inbox = Inbox::new();
    let token = cancel::Token::new();
    inbox.push_user("old".into());
    let drawn = inbox
        .drain_drawn(&inbox.mailbox().stamp(), &token)
        .pop()
        .unwrap();
    let (hold, _, ()) = inbox.mailbox().begin_abandon(Some(&token), || ()).unwrap();
    drop(hold);
    assert!(!drawn.stamp.current(&token));
}

#[test]
fn an_abandoned_deliberation_cannot_drain_fresh_followups() {
    let inbox = Inbox::new();
    let mailbox = inbox.mailbox();
    let token = cancel::Token::new();
    let work = mailbox.stamp();
    let (hold, _, ()) = mailbox.begin_abandon(Some(&token), || ()).unwrap();
    drop(hold);
    mailbox.steer("fresh".into());
    assert!(inbox.drain_drawn(&work, &token).is_empty());
    assert_eq!(mailbox.queued_human_messages(), vec!["fresh"]);
    assert!(mailbox.stamp().reset(&token));
    assert!(inbox.drain_drawn(&work, &token).is_empty());
    let drawn = inbox.draw(|| ParkMode::Quiesce, &token).unwrap();
    assert!(drawn.stamp.reset(&token));
    assert!(matches!(drawn.next, Next::Item(Item::Human(text)) if text == "fresh"));
}

#[test]
fn admission_holds_preserve_delivery_stamps() {
    let inbox = Inbox::new();
    let mailbox = inbox.mailbox();
    let token = cancel::Token::new();
    let stamp = mailbox.stamp();
    let (hold, ()) = mailbox.hold(|| ());
    assert!(!stamp.current(&token));
    drop(hold);
    assert!(stamp.current(&token));
}

#[test]
fn admission_holds_reopen_during_unwind() {
    let inbox = Inbox::new();
    let mailbox = inbox.mailbox();
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _hold = mailbox.hold(|| panic!("snapshot failed"));
    }));
    assert!(failed.is_err());
    assert_eq!(inbox.shared.lock().stopping, 0);
}

#[test]
fn terminal_consumers_refuse_steering_and_captures() {
    let inbox = Inbox::new();
    let mailbox = inbox.mailbox();
    let token = cancel::Token::new();
    let stamp = mailbox.stamp();
    mailbox.admission(|| token.cancel(CancelCause::Cancelled));
    assert!(!mailbox.steer_live("late".into(), &token));
    assert!(stamp.admit(&token, || ()).is_none());
    assert!(inbox.is_empty());
}
