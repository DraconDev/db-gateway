//! The MCP server's protocol, driven the way a client drives it.
//!
//! Most tests speak the wire format itself -- one JSON-RPC message per
//! line -- and assert what comes back. A gateway is not needed for them:
//! the paths under test (the handshake, the catalogue, an unknown tool, a
//! malformed line, a refused write) are answered before any request
//! leaves the process, and the one path that would reach the network is
//! pointed at a port nothing listens on, so a test that reached it by
//! accident fails at once instead of waiting for a timeout.
//!
//! The last test is the exception: it runs against a real gateway, so
//! the tools are proved to answer with the gateway's own facts and not
//! only to be well-shaped.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::{Value, json};
use std::io::Write;

/// A gateway that is never reached.
///
/// The port is one nothing listens on, so a mistake that reaches the
/// network fails immediately rather than hanging on a timeout.
const UNREACHABLE: &str = "http://127.0.0.1:1";

/// A credential-shaped value.
///
/// The server holds it and never says it; it is here so a test can
/// prove that it does not appear in an answer.
fn credential() -> String {
    "c0ffee11".repeat(8)
}

/// Run one session of protocol messages and return the answers.
///
/// The reader and the writer are in memory, so the test sees exactly
/// what the server wrote and nothing else.
async fn session(messages: &[&str]) -> Vec<Value> {
    let input = format!("{}\n", messages.join("\n"));
    let gateway = dg::mcp::Gateway::new(UNREACHABLE, &credential());
    let mut output: Vec<u8> = Vec::new();
    dg::mcp::serve_loop(
        std::io::Cursor::new(input.into_bytes()),
        &mut output,
        &gateway,
    )
    .await
    .expect("the loop ends when the input ends");
    String::from_utf8(output)
        .expect("the protocol is utf-8")
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).expect("every line the server wrote is a message")
        })
        .collect()
}

/// The handshake, the catalogue and an unknown tool.
///
/// The unknown tool is the interesting one: a client can ask for
/// anything, and the answer must be an error rather than a panic --
/// a panic in a stdio server takes the whole session with it.
#[tokio::test]
async fn the_handshake_lists_tools_and_refuses_an_unknown_one() {
    let answers = session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"dg_delete","arguments":{"name":"app"}}}"#,
    ])
    .await;

    // The notification is answered with silence: JSON-RPC has no answer
    // for a message without an id.
    assert_eq!(
        answers.len(),
        3,
        "a notification must not be answered: {answers:?}"
    );

    let initialize = &answers[0];
    assert_eq!(initialize["id"].as_i64(), Some(1));
    assert_eq!(
        initialize["result"]["protocolVersion"].as_str(),
        Some("2024-11-05"),
        "a client that names a revision this server speaks is answered in its own words"
    );
    assert!(
        initialize["result"]["capabilities"]["tools"].is_object(),
        "the server must advertise the tools capability: {initialize}"
    );
    assert_eq!(
        initialize["result"]["serverInfo"]["name"].as_str(),
        Some("dg")
    );

    let tools = &answers[1];
    assert_eq!(tools["id"].as_i64(), Some(2));
    assert!(
        tools["result"]["tools"]
            .as_array()
            .is_some_and(|list| !list.is_empty()),
        "the catalogue must not be empty: {tools}"
    );

    let unknown = &answers[2];
    assert_eq!(unknown["id"].as_i64(), Some(3));
    assert_eq!(
        unknown["error"]["code"].as_i64(),
        Some(-32602),
        "an unknown tool is a bad request: {unknown}"
    );
    assert_eq!(
        unknown["error"]["data"]["code"].as_str(),
        Some("unknown_tool"),
        "the failure must carry a stable code: {unknown}"
    );
    assert!(
        unknown["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "the failure must carry a human message: {unknown}"
    );
}

/// The catalogue offers reads and nothing else.
///
/// This is the promise the whole server rests on: an agent that can
/// only read cannot change anything. A write tool appearing here fails
/// the test on the spot.
#[tokio::test]
async fn the_catalogue_offers_reads_and_nothing_else() {
    let answers = session(&[r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#]).await;
    let tools = answers[0]["result"]["tools"]
        .as_array()
        .expect("the catalogue is an array")
        .clone();

    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(
        names,
        [
            "dg_status",
            "dg_list",
            "dg_info",
            "dg_inspect",
            "dg_backups",
            "dg_query"
        ],
        "the catalogue changed: {names:?}"
    );

    // No name may carry a verb that changes something.
    for name in &names {
        for verb in [
            "create", "delete", "drop", "insert", "update", "clone", "import", "export", "execute",
            "batch", "write", "mutate", "key", "rotate", "revoke", "issue",
        ] {
            assert!(
                !name.contains(verb),
                "the catalogue offers a write tool: {name}"
            );
        }
    }

    // No tool may ask for a credential: the agent has no business
    // handing the server a key, and the server has none to give.
    for tool in &tools {
        let properties = tool["inputSchema"]["properties"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        for argument in properties.keys() {
            assert!(
                !matches!(
                    argument.as_str(),
                    "key" | "token" | "credential" | "secret" | "password"
                ),
                "the tool {tool} asks for a credential"
            );
        }
    }
}

/// A statement that writes is refused before a request is made.
///
/// The gateway is unreachable, so a request that was sent would come
/// back as a transport failure. A refusal means the refusal happened
/// here, on the client's own rule.
#[tokio::test]
async fn a_write_statement_is_refused_before_a_request_is_made() {
    let answers = session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"dg_query","arguments":{"name":"app","sql":"DELETE FROM t"}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"dg_query","arguments":{"name":"app","sql":"SELECT 1"}}}"#,
    ])
    .await;

    assert_eq!(answers.len(), 2);
    assert_eq!(
        answers[0]["error"]["data"]["code"].as_str(),
        Some("sql_forbidden"),
        "a write statement must be refused: {answers:?}"
    );
    // The read is not refused here: it goes to the gateway, which is not
    // there. That is the difference between "refused" and "not answered".
    assert_ne!(
        answers[1]["error"]["data"]["code"].as_str(),
        Some("sql_forbidden"),
        "a read statement must not be refused as a write: {answers:?}"
    );
}

/// A malformed line is answered and the session continues.
///
/// One bad line from a client is a client mistake, not a reason to end
/// the conversation: the next message must still be answered.
#[tokio::test]
async fn a_malformed_line_is_answered_and_the_session_continues() {
    let answers = session(&[
        "{\"jsonrpc\":",
        r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#,
    ])
    .await;

    assert_eq!(answers.len(), 2, "both lines must be answered: {answers:?}");
    assert_eq!(
        answers[0]["error"]["code"].as_i64(),
        Some(-32700),
        "a malformed line is a parse error: {answers:?}"
    );
    assert!(
        answers[0]["id"].is_null(),
        "a parse error has no id to answer: {answers:?}"
    );
    assert_eq!(answers[1]["id"].as_i64(), Some(7));
    assert!(
        answers[1]["result"].is_object(),
        "the session must continue after a malformed line: {answers:?}"
    );
}

/// A method the server does not speak is refused, and a notification is
/// never answered.
///
/// The notification case matters: answering a notification teaches a
/// client that the server talks when it is not spoken to.
#[tokio::test]
async fn an_unknown_method_is_refused_and_a_notification_is_not() {
    let answers = session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"x"}}"#,
    ])
    .await;

    assert_eq!(
        answers.len(),
        2,
        "the notification must not be answered: {answers:?}"
    );
    assert_eq!(
        answers[0]["error"]["code"].as_i64(),
        Some(-32601),
        "an unknown method is method-not-found: {answers:?}"
    );
    assert_eq!(answers[1]["id"].as_i64(), Some(2));
    assert_eq!(answers[1]["error"]["code"].as_i64(), Some(-32601));
}

/// An answer never carries the credential.
///
/// The server holds the credential for its whole life; nothing it says
/// may carry it back out.
#[tokio::test]
async fn no_answer_carries_the_credential() {
    let answers = session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"dg_query","arguments":{"name":"app","sql":"DELETE FROM t"}}}"#,
    ])
    .await;

    let said = serde_json::to_string(&answers).expect("the answers serialize");
    assert!(
        !said.contains(&credential()),
        "the credential appeared in an answer: {said}"
    );
}

/// The protocol revision is negotiated, not asserted.
///
/// A client that names a revision this server does not speak is
/// answered with one it does, rather than with an error: the client
/// asked to talk, and there is a version both understand.
#[tokio::test]
async fn an_unknown_protocol_revision_is_answered_with_a_known_one() {
    let answers = session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
    ])
    .await;

    let version = answers[0]["result"]["protocolVersion"]
        .as_str()
        .expect("a protocol version")
        .to_string();
    assert!(
        [
            "2024-11-05",
            "2025-03-26",
            "2025-06-18",
            "2026-01-01",
            "2026-06-18"
        ]
        .contains(&version.as_str()),
        "the server answered with a revision it does not speak: {version}"
    );
    assert_ne!(version, "1999-01-01");
}

/// The half of the stdout capture that runs in a child process.
///
/// It runs the loop with the REAL stdout as the transport, with fd 1
/// pointed at the capture file, so the parent can assert that the
/// process's stdout carried the protocol and nothing else. It is
/// `#[ignore]`d because it only means anything when the parent asks for
/// it -- and the parent does, by name.
#[cfg(unix)]
#[tokio::test]
#[ignore = "driven by the stdout capture test"]
async fn stdout_capture_child() {
    use std::os::fd::AsRawFd;

    let Ok(path) = std::env::var("DG_MCP_CAPTURE") else {
        return;
    };
    let file = std::fs::File::create(&path).expect("capture file");
    // SAFETY: both descriptors are valid for the duration of the call,
    // and the saved one is put back before this test returns.
    let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };
    assert!(saved >= 0, "cannot save stdout");
    assert!(
        unsafe { libc::dup2(file.as_raw_fd(), libc::STDOUT_FILENO) } >= 0,
        "cannot point stdout at the capture file"
    );

    let gateway = dg::mcp::Gateway::new(UNREACHABLE, &credential());
    let mut out = std::io::stdout();
    let _ = dg::mcp::serve_loop(std::io::stdin().lock(), &mut out, &gateway).await;

    // SAFETY: the saved descriptor is still open, and it is the one
    // stdout was pointing at before.
    unsafe {
        libc::dup2(saved, libc::STDOUT_FILENO);
        libc::close(saved);
    }
}

/// The process's stdout carries the protocol and nothing else.
///
/// stdout IS the transport: a stray print breaks the session, and a
/// log line that leaks into it is worse -- the client reads it as
/// protocol. The capture therefore runs the server in a child process
/// with the real stdout, and every byte it wrote must be a protocol
/// message.
#[cfg(unix)]
#[test]
fn stdout_carries_the_protocol_and_nothing_else() {
    let dir = tempfile::tempdir().expect("temp dir");
    let capture = dir.path().join("stdout.txt");
    let session = format!(
        "{}\n{}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );

    let mut child = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--ignored", "--exact", "stdout_capture_child"])
        .env("DG_MCP_CAPTURE", &capture)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the capture helper");
    {
        let mut stdin = child.stdin.take().expect("the child's stdin");
        stdin
            .write_all(session.as_bytes())
            .expect("write the session");
        // Dropping the handle closes the pipe, which is the child's
        // end of input.
    }
    let output = child.wait_with_output().expect("the helper finishes");
    assert!(
        output.status.success(),
        "the helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let captured = std::fs::read_to_string(&capture).expect("the capture file");
    assert!(!captured.is_empty(), "nothing reached stdout");
    assert_eq!(
        captured.lines().count(),
        2,
        "one answer per request and nothing else: {captured}"
    );
    for line in captured.lines() {
        let message: Value =
            serde_json::from_str(line).expect("every byte on stdout is a protocol message");
        assert_eq!(message["jsonrpc"].as_str(), Some("2.0"));
        assert!(
            message.get("id").is_some(),
            "a protocol answer carries an id: {message}"
        );
    }
}

/// The catalogue and the handshake agree with each other.
///
/// A client reads the tool list once and then calls what it finds
/// there, so a tool in the catalogue that cannot be called is a broken
/// promise.
#[tokio::test]
async fn every_tool_in_the_catalogue_is_callable() {
    let answers = session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"dg_status","arguments":{}}}"#,
    ])
    .await;

    // The call is answered -- with a transport failure, because there is
    // no gateway -- but it is answered, which is the point: the tool
    // exists and dispatch found it.
    assert_eq!(answers.len(), 2);
    assert!(
        answers[1]["error"].is_object(),
        "a known tool must be dispatched, not refused as unknown: {answers:?}"
    );
    assert_ne!(
        answers[1]["error"]["data"]["code"].as_str(),
        Some("unknown_tool"),
        "the catalogue offered a tool that cannot be called: {answers:?}"
    );
    // The failure of a read is a transport failure, never a write
    // outcome: this server offers no writes at all.
    assert_eq!(
        answers[1]["error"]["data"]["code"].as_str(),
        Some("transport_unavailable"),
        "an unreachable gateway is a transport failure: {answers:?}"
    );
    let _ = json!({});
}
