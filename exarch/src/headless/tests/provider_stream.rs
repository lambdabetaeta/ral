use super::*;
use crate::agent::{Admission, LaunchConfig, RootEngine};
use crate::app::{EXARCH, Scratch};
use crate::provider::{Billing, Bureau, Pool, Toolset};
use crate::record::{View, context};
use genai::chat::{ChatMessage, ContentPart};
use ral_core::own::OwnDir;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

// One wire fixture family exercises both completion content and retry evidence.
struct Sse {
    endpoint: String,
    stopped: Arc<AtomicBool>,
    server: Option<JoinHandle<Vec<Value>>>,
}

impl Sse {
    fn serve(replies: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/v1/", listener.local_addr().unwrap());
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for reply in replies {
                let deadline = Instant::now() + Duration::from_secs(20);
                let mut stream = loop {
                    if stop.load(Ordering::Acquire) {
                        return requests;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "missing fixture request");
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("loopback accept: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(WAIT)).unwrap();
                stream.set_write_timeout(Some(WAIT)).unwrap();
                requests.push(read_request(&mut stream));
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).unwrap();
            }
            requests
        });
        Self {
            endpoint,
            stopped,
            server: Some(server),
        }
    }

    fn provider(&self, billing: Billing) -> Arc<Provider> {
        Arc::new(Pool::loopback_for_test(self.endpoint.clone(), billing))
    }

    fn finish(mut self) -> Vec<Value> {
        self.server.take().unwrap().join().unwrap()
    }
}

impl Drop for Sse {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let header_end = loop {
        if let Some(at) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break at + 4;
        }
        let read = stream.read(&mut buffer).unwrap();
        assert!(
            read > 0 && bytes.len() < 64 * 1024,
            "incomplete HTTP headers"
        );
        bytes.extend_from_slice(&buffer[..read]);
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
    assert!(headers.starts_with("POST /v1/messages "), "{headers}");
    let length: usize = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .unwrap()
        .1
        .trim()
        .parse()
        .unwrap();
    assert!(length < 2 * 1024 * 1024);
    while bytes.len() < header_end + length {
        let read = stream.read(&mut buffer).unwrap();
        assert!(read > 0, "incomplete HTTP body");
        bytes.extend_from_slice(&buffer[..read]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
}

fn event(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn start(reported: bool) -> String {
    let mut message = json!({"id":"fixture", "role":"assistant", "content":[]});
    if reported {
        message["usage"] = json!({
            "input_tokens":10, "output_tokens":0,
            "cache_creation_input_tokens":2, "cache_read_input_tokens":4,
        });
    }
    event(
        "message_start",
        &json!({"type":"message_start", "message":message}),
    )
}

fn thinking(index: usize, text: &str, signature: &str) -> String {
    [
        event(
            "content_block_start",
            &json!({"index":index,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        ),
        event(
            "content_block_delta",
            &json!({"index":index,"delta":{"type":"thinking_delta","thinking":text}}),
        ),
        event(
            "content_block_delta",
            &json!({"index":index,"delta":{"type":"signature_delta","signature":signature}}),
        ),
        event("content_block_stop", &json!({"index":index})),
    ]
    .concat()
}

fn tool_start(index: usize, id: &str) -> String {
    event(
        "content_block_start",
        &json!({"index":index,"content_block":{"type":"tool_use","id":id,"name":"ral","input":{}}}),
    )
}

fn tool_delta(index: usize, arguments: &str) -> String {
    event(
        "content_block_delta",
        &json!({"index":index,"delta":{"type":"input_json_delta","partial_json":arguments}}),
    )
}

fn completed(pairs: &[(&str, &str)], id: &str, command: &str, reported: bool) -> String {
    let mut sse = start(reported);
    for (index, (text, signature)) in pairs.iter().enumerate() {
        sse.push_str(&thinking(index, text, signature));
    }
    let index = pairs.len();
    sse.push_str(&tool_start(index, id));
    sse.push_str(&tool_delta(
        index,
        &json!({"command":command,"description":"fixture"}).to_string(),
    ));
    sse.push_str(&event("content_block_stop", &json!({"index":index})));
    let mut end = json!({"delta":{"stop_reason":"tool_use"}});
    if reported {
        end["usage"] = json!({"output_tokens":3});
    }
    sse.push_str(&event("message_delta", &end));
    sse.push_str(&event("message_stop", &json!({"type":"message_stop"})));
    sse
}

fn reply(reported: bool) -> String {
    completed(&[], "reply", "exarch-agents `reply 'safe'", reported)
}

#[derive(Clone, Copy)]
enum FailedContent {
    ToolStart,
    Tool,
    Signature,
    Mixed,
    Heartbeat,
    Empty,
}

fn failed(content: FailedContent) -> String {
    let mut sse = start(false);
    match content {
        FailedContent::ToolStart => sse.push_str(&tool_start(0, "abandoned")),
        FailedContent::Tool => {
            sse.push_str(&tool_start(0, "abandoned"));
            sse.push_str(&tool_delta(0, r#"{"command":"let leaked ="#));
        }
        FailedContent::Signature => sse.push_str(&thinking(0, "", "opaque-only")),
        FailedContent::Mixed => {
            sse.push_str(&thinking(0, "not answer prose", "opaque-one"));
            sse.push_str(&thinking(1, "more reasoning", "opaque-two"));
            sse.push_str(&tool_start(2, "abandoned"));
            sse.push_str(&tool_delta(2, "{"));
            sse.push_str(&tool_delta(2, r#""command":"#));
        }
        FailedContent::Heartbeat => {
            sse.push_str(": keepalive\n\n");
            sse.push_str(&event("ping", &json!({"type":"ping"})));
        }
        FailedContent::Empty => {
            sse.push_str(&thinking(0, "", ""));
            sse.push_str(&event(
                "content_block_start",
                &json!({"index":1,"content_block":{"type":"text","text":""}}),
            ));
            sse.push_str(&event(
                "content_block_delta",
                &json!({"index":1,"delta":{"type":"text_delta","text":""}}),
            ));
        }
    }
    sse.push_str(&event("error", &json!({"type":"error","error":{"type":"overloaded_error","code":503,"message":"fixture overload"}})));
    sse
}

fn config(run_dir: OwnDir) -> LaunchConfig {
    LaunchConfig {
        system: "system".into(),
        run_dir,
        returns: true,
        admission: Admission::Free,
        helper_idle: crate::host::HELPER_IDLE,
        tools: Toolset::offered(false),
        max_tokens: None,
        allow_schedule: false,
        resume_on_reset: false,
        disk_warn_bytes: None,
        fuel: 0,
        search: false,
        factory: None,
        bureau: Arc::new(Bureau::scripted([])),
        roles: crate::provider::roles::Roles::default(),
    }
}

fn engine(scratch: Arc<Scratch>) -> RootEngine {
    RootEngine::Identity {
        scratch,
        cwd: ral_core::host::cwd().unwrap(),
        grants: ral_core::authority::policy::GrantStack::default(),
        terminal: ral_core::io::terminal::TerminalState::default(),
    }
}

fn run_json(host: &Host, provider: &Provider) -> (Value, String) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    run_to(
        host,
        &SessionInfo {
            system_size: 0,
            system_files: &[],
            base: "dangerous",
            extend: None,
            restrict_files: &[],
            cwd: &ral_core::host::cwd().unwrap(),
        },
        provider,
        Some("finish safely".into()),
        OutputFormat::Json,
        (&mut out, &mut err),
    )
    .unwrap();
    let result: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(result["is_error"], false);
    assert_eq!(result["result"], "safe");
    (result, String::from_utf8(err).unwrap())
}

fn replay_usage(sessions: &OwnDir, id: AgentId) -> Usage {
    let dir = sessions.dir(&id.to_string()).unwrap();
    let path = dir.path().join("record.jsonl");
    record::replay::<View>(dir.open("record.jsonl").unwrap(), &path, Blocks::default())
        .unwrap()
        .usage()
}

fn assert_usage_eq(left: Usage, right: Usage) {
    assert_eq!(
        serde_json::to_value(left).unwrap(),
        serde_json::to_value(right).unwrap()
    );
}

fn signed_parts(message: &ChatMessage) -> Vec<(&str, &str)> {
    let mut parts = message.content.iter();
    let mut pairs = Vec::new();
    while let Some(part) = parts.next() {
        match part {
            ContentPart::ThoughtSignature(signature) => {
                let Some(ContentPart::ReasoningContent(text)) = parts.next() else {
                    panic!("a signature must precede exactly its reasoning");
                };
                pairs.push((text.as_str(), signature.as_str()));
            }
            ContentPart::ReasoningContent(_) => panic!("extra aggregate reasoning"),
            _ => {}
        }
    }
    pairs
}

fn assert_wire_pairs(request: &Value, expected: &[(&str, &str)]) {
    let messages = request["messages"].as_array().unwrap();
    let at = messages
        .iter()
        .position(|message| {
            message["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part["id"] == "value"))
        })
        .expect("the signed assistant tool turn reaches the native adapter");
    let parts = messages[at]["content"].as_array().unwrap();
    assert_eq!(parts.len(), expected.len() + 1);
    for (part, (text, signature)) in parts.iter().zip(expected) {
        assert_eq!(part["type"], "thinking");
        assert_eq!(part["thinking"], *text);
        assert_eq!(part["signature"], *signature);
    }
    assert_eq!(parts.last().unwrap()["type"], "tool_use");
    let result = &messages[at + 1]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "value");
    let content = result["content"].as_str().unwrap();
    assert!(content.contains("VALUE:\n42\n"), "{content}");
    assert!(content.contains("EXIT: 0"), "{content}");
}

#[test]
fn signed_streams_survive_durable_admission_resume_and_native_continuation() {
    for pairs in [
        vec![("only once", "opaque+/=\n签名")],
        vec![("first", "signature-1"), ("second", "signature-2")],
        vec![("equal text", "signature-a"), ("equal text", "signature-b")],
    ] {
        let server = Sse::serve(vec![
            completed(&pairs, "value", "return $[40 + 2]", true),
            reply(true),
            reply(true),
        ]);
        let provider = server.provider(Billing::Metered);
        let scratch = Arc::new(Scratch::for_test(EXARCH, "signed-stream").unwrap());
        let run_dir = scratch.test_sibling("run").unwrap();
        let host = Host::found(
            config(run_dir.clone()),
            engine(scratch.clone()),
            provider.clone(),
        )
        .unwrap();
        let id = host.children()[0];
        let sessions = run_dir.dir("sessions").unwrap();
        let (json, _) = run_json(&host, &provider);
        let live = host.port().usage();
        assert_eq!(json["usage"]["unreported_turns"], 0);
        assert_eq!(live.unreported, 0);
        assert_eq!(live.input, 32);
        assert_eq!(live.output, 6);
        assert_usage_eq(replay_usage(&sessions, id), live);
        assert_eq!(host.end(), [] as [String; 0]);

        let records = record::read_records(
            &sessions
                .dir(&id.to_string())
                .unwrap()
                .path()
                .join("record.jsonl"),
        )
        .unwrap();
        let canonical = records
            .iter()
            .find_map(|record| match record {
                Record::Transcript(Transcript::AssistantMessage {
                    message,
                    pending_tool_ids,
                    ..
                }) if pending_tool_ids == &["value"] => Some(message),
                _ => None,
            })
            .unwrap();
        assert_eq!(signed_parts(canonical), pairs);
        let (resumed, _) = context::resume(&sessions, id).unwrap();
        let rendered = resumed.rendered();
        let stored = rendered
            .iter()
            .find(|message| !signed_parts(message).is_empty())
            .unwrap();
        assert_eq!(
            serde_json::to_value(stored).unwrap(),
            serde_json::to_value(canonical).unwrap()
        );
        let canonical_weight: usize = rendered
            .iter()
            .map(|message| serde_json::to_vec(message).unwrap().len())
            .sum();
        assert_eq!(resumed.history_bytes(), canonical_weight);
        let row = resumed
            .context_survey()
            .rows
            .into_iter()
            .find(|row| row.span.from() == 2)
            .unwrap();
        assert_eq!(
            row.bytes,
            rendered[1..3]
                .iter()
                .map(|message| serde_json::to_vec(message).unwrap().len())
                .sum::<usize>()
        );

        let host = Host::resume(config(run_dir), engine(scratch), &provider).unwrap();
        run_json(&host, &provider);
        assert_eq!(host.end(), [] as [String; 0]);
        let requests = server.finish();
        assert_eq!(requests.len(), 3);
        assert_wire_pairs(&requests[1], &pairs);
        assert_wire_pairs(&requests[2], &pairs);
    }
}

#[test]
fn retried_generated_streams_record_one_unknown_attempt_and_meter_it_once() {
    use FailedContent::{Empty, Heartbeat, Mixed, Signature, Tool, ToolStart};
    for (failures, unknown) in [
        (vec![ToolStart], 1),
        (vec![Tool], 1),
        (vec![Signature], 1),
        (vec![Mixed], 1),
        (vec![Tool, Signature], 2),
        (vec![Heartbeat], 0),
        (vec![Empty], 0),
    ] {
        let mut replies: Vec<_> = failures.iter().copied().map(failed).collect();
        replies.push(completed(
            &[("done", "signed-success")],
            "reply",
            "exarch-agents `reply 'safe'",
            true,
        ));
        let server = Sse::serve(replies);
        let provider = server.provider(Billing::Metered);
        let mut root = testkit::trunk(true);
        set_provider(&mut root, provider.clone());
        let id = root.id();
        let sessions = root.run_dir().dir("sessions").unwrap();
        let host = testkit::host(root);
        let (result, _) = run_json(&host, &provider);
        let live = host.port().usage();
        assert_eq!(live.input, 16);
        assert_eq!(live.output, 3);
        assert_eq!(live.cache_creation, Some(2));
        assert_eq!(live.cache_read, Some(4));
        assert_eq!(live.unreported, unknown);
        assert_eq!(result["usage"]["unreported_turns"], unknown);
        assert_eq!(result["usage"]["input_tokens"], 16);
        assert_eq!(result["total_cost_usd"].is_null(), unknown > 0);
        assert_usage_eq(replay_usage(&sessions, id), live);
        let path = sessions
            .dir(&id.to_string())
            .unwrap()
            .path()
            .join("record.jsonl");
        let records = record::read_records(&path).unwrap();
        let usage: Vec<_> = records
            .iter()
            .filter_map(|record| match record {
                Record::Forensic(Forensic::UsageDelta { usage, .. }) => Some(*usage),
                _ => None,
            })
            .collect();
        assert_eq!(usage.len(), usize::try_from(unknown).unwrap() + 1);
        assert_eq!(
            usage.iter().filter(|usage| usage.unreported == 1).count(),
            usize::try_from(unknown).unwrap()
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, Record::Forensic(Forensic::Retried { .. })))
                .count(),
            failures.len()
        );
        let assistants: Vec<_> = records
            .iter()
            .filter_map(|record| match record {
                Record::Transcript(Transcript::AssistantMessage {
                    pending_tool_ids, ..
                }) => Some(pending_tool_ids),
                _ => None,
            })
            .collect();
        assert_eq!(assistants.iter().filter(|ids| !ids.is_empty()).count(), 1);
        assert!(!records.iter().any(|record| matches!(record, Record::Display(Display::ToolCall { cmd, .. }) if cmd.contains("leaked"))));
        assert_eq!(host.end(), [] as [String; 0]);
        let requests = server.finish();
        assert_eq!(requests.len(), failures.len() + 1);
        assert!(
            requests
                .iter()
                .all(|request| !request["messages"].to_string().contains("abandoned"))
        );
    }
}

#[test]
fn successful_signed_tool_streams_account_only_the_end_report_or_its_absence() {
    for billing in [Billing::Metered, Billing::FlatRate] {
        for reported in [true, false] {
            let server = Sse::serve(vec![completed(
                &[("done", "signature")],
                "reply",
                "exarch-agents `reply 'safe'",
                reported,
            )]);
            let provider = server.provider(billing);
            let mut root = testkit::trunk(true);
            set_provider(&mut root, provider.clone());
            let id = root.id();
            let sessions = root.run_dir().dir("sessions").unwrap();
            let host = testkit::host(root);
            let (result, err) = run_json(&host, &provider);
            let live = host.port().usage();
            assert_eq!(live.unreported, u32::from(!reported));
            assert_eq!(live.unmetered, billing == Billing::FlatRate);
            assert_eq!(live.input, if reported { 16 } else { 0 });
            assert_eq!(result["usage"]["unreported_turns"], u32::from(!reported));
            assert_eq!(result["total_cost_usd"].is_null(), !reported);
            if billing == Billing::FlatRate && !reported {
                assert!(err.contains("subscription (usage unknown)"), "{err}");
            }
            assert_usage_eq(replay_usage(&sessions, id), live);
            let records = record::read_records(
                &sessions
                    .dir(&id.to_string())
                    .unwrap()
                    .path()
                    .join("record.jsonl"),
            )
            .unwrap();
            assert_eq!(
                records
                    .iter()
                    .filter(|record| matches!(
                        record,
                        Record::Forensic(Forensic::UsageDelta { .. })
                    ))
                    .count(),
                1
            );
            assert_eq!(host.end(), [] as [String; 0]);
            assert_eq!(server.finish().len(), 1);
        }
    }
}
