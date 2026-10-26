#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use common::pty::PtySession;
use std::process::Command;
use std::sync::Barrier;
use std::time::Duration;

#[test]
fn parallel_sessions_own_distinct_terminals() {
    let start = Barrier::new(8);
    std::thread::scope(|scope| {
        for worker in 0..8 {
            let start = &start;
            scope.spawn(move || {
                start.wait();
                for round in 0..8 {
                    let mut session = PtySession::spawn_command(Command::new("/bin/cat"))
                        .expect("create an independent terminal");
                    assert_eq!(session.foreground_pgid(), Some(session.pid()));
                    let line = format!("terminal-{worker}-{round}");
                    session.send_line(&line).unwrap();
                    assert!(
                        session.wait_until(Duration::from_secs(5), |text| {
                            text.matches(&line).count() == 2
                        }),
                        "{line} was not echoed by both its terminal and its child: {}",
                        session.text()
                    );
                    session.send_byte(4).unwrap();
                    assert!(
                        session
                            .wait_for_exit(Duration::from_secs(5))
                            .unwrap()
                            .success()
                    );
                }
            });
        }
    });
}
