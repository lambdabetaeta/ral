#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use common::pty::PtySession;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;
use unicode_width::UnicodeWidthStr;

const WAIT: Duration = Duration::from_secs(15);
const PROMPT: &str = "P> ";

fn session(dir: &TempDir, cols: usize, rc: &str) -> PtySession {
    std::fs::create_dir_all(dir.path().join("ral")).unwrap();
    std::fs::write(dir.path().join("ral/rc"), rc).unwrap();
    // Set the size before rustyline starts, not after its first layout.
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "stty cols \"$1\" rows 40; shift; exec \"$@\"", "pty"])
        .arg(cols.to_string())
        .arg(common::ral_bin())
        .args(["-i", "--surface", "readline"])
        .current_dir(dir.path())
        .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path())
        .env("XDG_STATE_HOME", dir.path())
        .env(
            "RAL_PATH",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins"),
        )
        .env("TERM", "xterm-256color")
        .env("TERM_PROGRAM", "WezTerm")
        .env("RAL_INTERACTIVE_MODE", "full");
    let mut session = PtySession::spawn_command(command).unwrap();
    prompt(&mut session, 1);
    session
}

fn prompt(session: &mut PtySession, count: usize) {
    assert!(
        session.wait_until(WAIT, |text| {
            text.matches("\x1b[?2004h").count() >= count && text.contains(PROMPT)
        }),
        "readline did not reach prompt {count}: {:?}",
        session.text()
    );
}

fn send(session: &mut PtySession, text: &str) {
    for byte in text.bytes() {
        session.send_byte(byte).unwrap();
    }
}

fn paste(session: &mut PtySession, text: &str) {
    send(session, &format!("\x1b[200~{text}\x1b[201~"));
    assert!(
        session.wait_until(WAIT, |output| {
            ral_core::io::ansi::strip(output)
                .replace("\r\n", "\n")
                .contains(text)
        }),
        "paste was not drawn: {:?}",
        session.text()
    );
}

// Only the terminal operations emitted by these fixtures; unknown controls fail.
struct Screen {
    cols: usize,
    row: usize,
    col: usize,
    lines: Vec<Vec<char>>,
}

impl Screen {
    fn read(text: &str, cols: usize) -> Self {
        let mut screen = Self {
            cols,
            row: 0,
            col: 0,
            lines: vec![vec![' '; cols]; 40],
        };
        let text: String = text
            .split_inclusive('\n')
            .filter(|line| !ral_core::io::ansi::strip(line).starts_with("[[DEBUG]"))
            .collect();
        let mut rest = text.as_str();
        while !rest.is_empty() {
            if let Some(csi) = rest.strip_prefix("\x1b[") {
                let end = csi
                    .find(|c: char| ('@'..='~').contains(&c))
                    .expect("complete CSI");
                let (params, command) = csi.split_at(end);
                let n = params.parse::<usize>().unwrap_or(1).max(1);
                match command.as_bytes()[0] {
                    b'A' => screen.row = screen.row.saturating_sub(n),
                    b'B' => screen.row += n,
                    b'C' => screen.col = (screen.col.min(cols - 1) + n).min(cols - 1),
                    b'D' => screen.col = screen.col.min(cols - 1).saturating_sub(n),
                    b'J' if params.is_empty() || params == "0" => {
                        screen.lines[screen.row][screen.col.min(cols)..].fill(' ');
                        screen.lines[screen.row + 1..]
                            .iter_mut()
                            .for_each(|line| line.fill(' '));
                    }
                    b'K' if params.is_empty() || params == "0" => {
                        screen.lines[screen.row][screen.col.min(cols)..].fill(' ');
                    }
                    b'm' | b'h' | b'l' => (),
                    other => panic!("unexpected CSI {params}{:?}", char::from(other)),
                }
                rest = &command[1..];
            } else if let Some(osc) = rest.strip_prefix("\x1b]") {
                let end = osc.find('\x07').expect("BEL-terminated title");
                rest = &osc[end + 1..];
            } else {
                let ch = rest.chars().next().unwrap();
                match ch {
                    '\r' => screen.col = 0,
                    '\n' => screen.row += 1,
                    '\t' => screen.col = ((screen.col / 8 + 1) * 8).min(cols - 1),
                    '\x07' => (),
                    c if c.is_control() => panic!("unexpected terminal control {c:?}"),
                    _ => {
                        let end = rest.find(char::is_control).unwrap_or(rest.len());
                        screen.draw(&rest[..end]);
                        rest = &rest[end..];
                        continue;
                    }
                }
                rest = &rest[ch.len_utf8()..];
            }
        }
        screen
    }

    fn draw(&mut self, text: &str) {
        let mut width = 0;
        for (offset, ch) in text.char_indices() {
            let next = text[..offset + ch.len_utf8()].width();
            let cells = next.saturating_sub(width);
            width = next;
            if cells == 0 {
                continue;
            }
            if self.col + cells > self.cols {
                self.row += 1;
                self.col = 0;
            }
            self.lines[self.row][self.col] = ch;
            self.col += cells;
        }
    }

    fn lines(&self) -> Vec<String> {
        let mut lines: Vec<_> = self
            .lines
            .iter()
            .map(|line| line.iter().collect::<String>().trim_end().to_owned())
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines
    }
}

#[test]
fn plugin_reedit_erases_the_old_drawing_but_not_previous_output() {
    let mut failures = Vec::new();
    for cols in [16, 24] {
        let cases = [
            ("below edge", "x".repeat(cols - 4)),
            ("exact edge", "x".repeat(cols - 3)),
            ("past edge", "x".repeat(cols - 2)),
            ("tab at zero", "\n\tabcdefghijklmnopqrstuvwZ".to_owned()),
            ("tab before edge", format!("{}\t", "x".repeat(cols - 4))),
            (
                "LF after exact edge",
                format!("{}\nZ", "x".repeat(cols - 3)),
            ),
            (
                "final LF after exact edge",
                format!("{}\n", "x".repeat(cols - 3)),
            ),
            ("wide glyph at edge", format!("{}界", "x".repeat(cols - 4))),
            (
                "combining at edge",
                format!("{}e\u{301}", "x".repeat(cols - 4)),
            ),
            (
                "emoji at edge",
                format!("{}👩\u{200d}💻", "x".repeat(cols - 5)),
            ),
            (
                "emoji across edge",
                format!("{}👩\u{200d}💻Z", "x".repeat(cols - 4)),
            ),
        ];
        for (name, buffer) in cases {
            let dir = TempDir::new().unwrap();
            std::fs::write(
                dir.path().join("reedit.ral"),
                r"
                return [name: 'reedit', keybindings: [[key: 'ctrl-t', handler: { |ctx|
                    let st = _ed-get
                    to-string $st[text] > received
                    _ed-set [text: `set 'NEW', cursor: `set 3]
                    return ()
                }]]]
            ",
            )
            .unwrap();
            let mut session = session(
                &dir,
                cols,
                r"
                return [prompt: { 'P> ' }, startup: { echo KEEP }, plugins: ['./reedit.ral': [:]]]
            ",
            );
            paste(&mut session, &buffer);
            session.send_byte(20).unwrap();
            prompt(&mut session, 2);
            assert!(
                session.wait_until(WAIT, |text| text.contains("P> NEW")),
                "{}",
                session.text()
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("received")).unwrap(),
                buffer,
                "{name}, {cols} columns: the key must see the unexecuted buffer"
            );
            let lines = Screen::read(&session.text(), cols).lines();
            if lines != ["KEEP", "P> NEW"] {
                failures.push(format!(
                    "{name}, {cols} columns: {lines:?}\n{:?}",
                    session.text()
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn stock_zoxide_key_accepts_a_crlf_selector_path() {
    for ending in ["\\n", "\\r\\n"] {
        let dir = TempDir::new().unwrap();
        let destination = dir.path().join("selected directory");
        std::fs::create_dir(&destination).unwrap();
        let resolved = ral_core::path::shown(ral_core::test_helper::real(&destination).as_path());
        let selector = dir.path().join("zoxide");
        std::fs::write(&selector, format!("#!/bin/sh\ncase \"$1\" in\nquery) printf '%s{ending}' \"$SELECTED_DIRECTORY\" ;;\nadd) printf '%s' \"$3\" > \"$HOME/recorded-directory\" ;;\n*) exit 2 ;;\nesac\n")).unwrap();
        std::fs::set_permissions(&selector, std::fs::Permissions::from_mode(0o755)).unwrap();
        let rc = format!(
            "return [prompt: {{ 'P> ' }}, env: [SELECTED_DIRECTORY: '{}'], plugins: [zoxide: [:]]]",
            destination.display()
        );
        let mut session = session(&dir, 80, &rc);
        send(&mut session, "\x1bz");
        prompt(&mut session, 2);
        assert!(
            dir.path().join("recorded-directory").exists(),
            "the stock zoxide key did not change directory: {:?}",
            session.text()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("recorded-directory")).unwrap(),
            resolved
        );
        paste(&mut session, "to-string !{cwd} > ../actual-directory");
        session.send_byte(b'\n').unwrap();
        prompt(&mut session, 3);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("actual-directory")).unwrap(),
            resolved
        );
    }
}
