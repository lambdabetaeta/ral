use super::*;
use crate::record::Forensic;

fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "exarch-log-test-{name}-{}-{:?}.jsonl",
        std::process::id(),
        std::thread::current().id()
    ))
}

/// The readable mirror is the file a person opens after being told a
/// session would not start, so what matters is that it exists beside the
/// machine record, that it holds one line per fact, and that the line
/// says when, what and something of the detail — without a timestamp
/// anyone has to convert in their head.
#[test]
fn every_record_gets_one_readable_line_beside_the_jsonl() {
    let path = temp_path("readable-mirror");
    let log = Log::create(&path).expect("temp record log");
    for text in ["first", "second"] {
        let _locus = log
            .append(Record::Forensic(Forensic::Error { text: text.into() }))
            .expect("append");
    }

    let mirror = path.with_extension("log");
    let read = std::fs::read_to_string(&mirror).expect("the mirror sits beside record.jsonl");
    let lines: Vec<_> = read.lines().collect();
    assert_eq!(lines.len(), 2, "one line per record, got {read:?}");
    for (line, text) in lines.iter().zip(["first", "second"]) {
        assert!(
            line.contains("forensic/error"),
            "the class and kind name the fact: {line}"
        );
        assert!(
            line.contains(text),
            "the fact's own detail survives: {line}"
        );
        assert!(
            line.starts_with("20") && line.contains('-') && line.contains(':'),
            "the moment is a date a person reads, not milliseconds: {line}"
        );
    }
}

/// A record carrying a whole transcript must still be one line, or the
/// file's one promise — a fact per line — is worth nothing on exactly the
/// records a debugger cares about.
#[test]
fn a_multi_line_fact_is_still_one_line_in_the_mirror() {
    let path = temp_path("readable-flattened");
    let log = Log::create(&path).expect("temp record log");
    let _locus = log
        .append(Record::Forensic(Forensic::Error {
            text: "a stack trace\nwith several lines\nin it".to_string(),
        }))
        .expect("append");

    let read = std::fs::read_to_string(path.with_extension("log")).expect("the mirror");
    assert_eq!(
        read.lines().count(),
        1,
        "a record with newlines in it is still one line: {read:?}"
    );
}

#[test]
fn a_record_round_trips_through_the_entry_envelope() {
    let path = temp_path("round-trip");
    let log = Log::create(&path).expect("temp record log");
    let record = Record::Forensic(Forensic::Error {
        text: "boom".into(),
    });
    let _locus = log.append(record).expect("append");

    let back: Vec<_> = Log::read(&path).expect("read back").collect();
    assert_eq!(back.len(), 1);
    let recorded = back.into_iter().next().unwrap().expect("parses");
    assert!(matches!(
        recorded.into_value(),
        Record::Forensic(Forensic::Error { text }) if text == "boom"
    ));

    let bytes = std::fs::read(&path).unwrap();
    let line = String::from_utf8(bytes).unwrap();
    assert!(
        line.contains("\"at_unix_ms\""),
        "the line on disk must carry the Entry envelope: {line}"
    );
}

#[test]
fn a_pre_envelope_line_is_refused_by_name() {
    let path = temp_path("pre-envelope");
    let record = Record::Forensic(Forensic::Error {
        text: "boom".into(),
    });
    let mut line = serde_json::to_vec(&record).unwrap();
    line.push(b'\n');
    std::fs::write(&path, &line).unwrap();

    let back: Vec<_> = Log::read(&path)
        .expect("the file itself reads back")
        .collect();
    assert_eq!(back.len(), 1);
    let error = back.into_iter().next().unwrap().expect_err("no envelope");
    let text = error.to_string();
    assert!(text.contains("Entry"), "{text}");
    assert!(text.contains("older exarch"), "{text}");
}

#[test]
fn a_locus_round_trips_identically_through_append_and_read() {
    let path = temp_path("locus-round-trip");
    let log = Log::create(&path).expect("temp record log");
    let first = log
        .append(Record::Forensic(Forensic::Error {
            text: "first".into(),
        }))
        .expect("append");
    let second = log
        .append(Record::Forensic(Forensic::Error {
            text: "second".into(),
        }))
        .expect("append");

    let back: Vec<_> = Log::read(&path).expect("read back").collect();
    assert_eq!(back.len(), 2);
    let mut back = back.into_iter();
    let first_back = back.next().unwrap().expect("parses");
    let second_back = back.next().unwrap().expect("parses");
    assert_eq!(*first_back.locus(), first);
    assert_eq!(*second_back.locus(), second);
}

#[test]
fn a_mismatched_body_misses_the_digest() {
    let path = temp_path("digest-mismatch");
    let log = Log::create(&path).expect("temp record log");
    let _locus = log
        .append(Record::Forensic(Forensic::Error {
            text: "boom".into(),
        }))
        .expect("append");

    let recorded = Log::read(&path)
        .expect("read back")
        .next()
        .expect("one record")
        .expect("parses");
    assert_ne!(
        Locus::digest_of(b"something else"),
        recorded.locus().digest()
    );
}
