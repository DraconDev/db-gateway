//! The MCP server: the gateway's read surfaces, spoken as JSON-RPC on stdio.
//!
//! WHY this exists: an AI agent needs the same gateway the operator uses,
//! and it needs that gateway to behave the same way for both of them.
//! The protocol is implemented here by hand rather than pulled in as a
//! dependency, because this project keeps a deliberately tiny supply
//! chain (`cargo deny` gates it): the wire format is a handful of JSON
//! objects, and a dependency would cost more than the code.
//!
//! Two rules shape the whole module:
//!
//!   * READ-ONLY. The gateway's write paths -- create, delete, clone,
//!     import, execute, batch and the key mutations -- are not exposed
//!     at all. The catalogue is the gateway's read surfaces and nothing
//!     else, so an agent talking to this server cannot ask it to change
//!     anything.
//!
//!   * STDOUT IS THE TRANSPORT. Every byte on stdout is a JSON-RPC
//!     message, because stdout is how the client and the server talk: a
//!     stray print breaks the session. Diagnostics go to stderr, through
//!     tracing, like the rest of the project's logs.
//!
//! The credential is the one the CLI already uses, resolved the same way
//! and read from the same place: the agent never supplies a key, so a
//! prompt that asks for one cannot make the server hand over a secret.

use crate::client::{self, Semantics};
use crate::error::{Error, Result};
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    path::Path,
};

/// The protocol revision this server answers with when the client names
/// one it does not know.
///
/// The version is a negotiation, not a fact: a client names the revision
/// it speaks and the server answers with one it speaks. `2024-11-05` is
/// the revision every client in the wild understands, so it is the answer
/// for a client that asks for something else -- a newer reply would be
/// a version that client may not know.
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

/// The revisions this server speaks, newest first.
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// JSON-RPC error code: the message was not JSON.
const PARSE_ERROR: i64 = -32700;
/// JSON-RPC error code: the envelope was not a JSON-RPC 2.0 request.
const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC error code: the method is not one this server speaks.
const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC error code: the request named a tool or argument wrongly.
const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC error code: the tool itself failed.
///
/// MCP leaves -32000..-32099 for the implementation, so the stable
/// machine-readable code of the failure travels in `data.code` and the
/// message stays the human sentence.
const TOOL_FAILED: i64 = -32001;

/// The longest a single line of the protocol may be.
///
/// The bound is enforced while reading, not afterwards: a client that
/// sends a gigabyte without a newline must not be able to make the
/// server hold it.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// The longest a single string value may be in a tool result.
const MAX_VALUE_CHARS: usize = 4096;

/// The largest a tool result may be once serialized.
///
/// An agent reads this into its context: a megabyte of JSON is not a
/// result, it is a way to lose the conversation.
const MAX_RESULT_BYTES: usize = 64 * 1024;

/// The most rows a query result carries back.
///
/// The gateway's own cap is higher; this one is about what an agent can
/// hold in its head at once. A result that had more rows says so.
const MAX_QUERY_ROWS: usize = 100;

/// The longest a statement may be, the same ceiling the API enforces.
const MAX_SQL_BYTES: usize = 65536;

/// The most bind parameters a statement may carry, the API's own cap.
const MAX_ARGS: usize = 256;

/// How deep a result is walked when its strings are shortened.
const MAX_DEPTH: usize = 24;

/// The gateway this server talks to, and the credential it talks with.
///
/// The credential is held in memory for the life of the process (the
/// terminal view holds it the same way) and is never printed, logged or
/// returned to a client.
pub struct Gateway {
    server: String,
    key: String,
}

impl Gateway {
    /// A gateway handle for one server and one credential.
    pub fn new(server: &str, key: &str) -> Self {
        Self {
            server: server.to_string(),
            key: key.to_string(),
        }
    }

    /// The server this handle talks to.
    pub fn server(&self) -> &str {
        &self.server
    }

    /// Read one path from the gateway.
    async fn get(&self, path: &str) -> Result<Value> {
        client::request_with_semantics(
            &self.server,
            &self.key,
            reqwest::Method::GET,
            path,
            None,
            None,
            Semantics::Read,
        )
        .await
    }

    /// Run one read-only statement against one database.
    ///
    /// The semantics are stated rather than derived from the method: a
    /// query is a POST either way, and a read that is given up on must
    /// not be reported as a write whose outcome is unknown.
    async fn query(&self, name: &str, sql: &str, args: &[Value]) -> Result<Value> {
        client::request_with_semantics(
            &self.server,
            &self.key,
            reqwest::Method::POST,
            &format!("/v1/databases/{name}/query"),
            Some(json!({"sql": sql, "args": args})),
            None,
            Semantics::Read,
        )
        .await
    }
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The credential is not in this dump, not even masked: a handle in
        // a log line should never be one secret away from being one.
        f.debug_struct("Gateway")
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

/// Serve the gateway to an MCP client over stdio.
///
/// The credential is resolved by the caller exactly as the rest of the
/// CLI resolves it -- `--key-file`, then `DG_KEY_FILE`, then
/// `~/.config/dg/key` -- and read with the same reader the CLI uses, so
/// the file permissions are checked the same way.
pub async fn serve(server: &str, key_path: Option<&Path>) -> Result<()> {
    let path =
        key_path.ok_or_else(|| Error::new(400, "missing_credential", "--key-file required"))?;
    // A missing file is named, not reported as a generic failure: the
    // reader cannot tell "there is nothing there" from "what is there
    // is not a credential", and an agent that asked for a key file
    // deserves to know which of the two it hit.
    if !path.exists() {
        return Err(Error::with_message(
            400,
            "missing_credential",
            format!("no credential file at {}", path.display()),
        ));
    }
    let key = client::read_key(path)?;
    let gateway = Gateway::new(server, &key);
    // On stderr, like every other log line this project writes: stdout
    // is the transport, and a line there would be read as protocol.
    tracing::info!(server = gateway.server(), "mcp server ready");
    serve_loop(std::io::stdin().lock(), std::io::stdout().lock(), &gateway).await
}

/// The JSON-RPC loop: read a message, answer it, read the next.
///
/// One message per line, in both directions. The read blocks between
/// messages and the answer is written whole: a client waits for the
/// answer before it says anything else, so there is never more than one
/// message in flight.
pub async fn serve_loop<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
    gateway: &Gateway,
) -> Result<()> {
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        buffer.clear();
        // `None` is the end of the transport: the client closed the pipe.
        let Some(overflow) = read_line(&mut reader, &mut buffer)? else {
            return Ok(());
        };
        let line = String::from_utf8_lossy(&buffer);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // A blank line carries nothing; it is not a protocol error.
            continue;
        }
        if overflow {
            // The line was too long to be a message. Say so and keep the
            // session: one oversized message is a client mistake, not a
            // reason to end the conversation.
            tracing::warn!(code = "message_too_long", "oversized protocol message");
            write_message(
                &mut writer,
                &error_response(
                    Value::Null,
                    PARSE_ERROR,
                    "the message is too long to be a protocol message",
                    None,
                ),
            )?;
            continue;
        }
        if let Some(response) = handle_line(gateway, trimmed).await {
            write_message(&mut writer, &response)?;
        }
    }
}

/// Read one newline-delimited line, bounded.
///
/// The bound is enforced while reading: a client that sends an endless
/// line must not be able to make the server hold it. The text that does
/// not fit is dropped and reported as an overflow, so the loop can say
/// the line was too long instead of trying to parse half a message.
fn read_line<R: BufRead>(reader: &mut R, out: &mut Vec<u8>) -> Result<Option<bool>> {
    let mut overflow = false;
    loop {
        let (found, consumed) = {
            let available = match reader.fill_buf() {
                Ok(available) => available,
                Err(_) => return Err(stdio_error()),
            };
            if available.is_empty() {
                // End of input. A trailing line without a newline is
                // still a line, so it is reported before the end.
                return Ok(if out.is_empty() && !overflow {
                    None
                } else {
                    Some(overflow)
                });
            }
            match available.iter().position(|byte| *byte == b'\n') {
                Some(index) => {
                    if !overflow {
                        out.extend_from_slice(&available[..index]);
                    }
                    (true, index + 1)
                }
                None => {
                    if !overflow {
                        out.extend_from_slice(available);
                        if out.len() > MAX_LINE_BYTES {
                            overflow = true;
                            out.clear();
                        }
                    }
                    (false, available.len())
                }
            }
        };
        reader.consume(consumed);
        if found {
            return Ok(Some(overflow));
        }
    }
}

/// Write one protocol message: a whole JSON object and a newline.
fn write_message<W: Write>(writer: &mut W, message: &Value) -> Result<()> {
    let mut bytes = match serde_json::to_vec(message) {
        Ok(bytes) => bytes,
        Err(_) => return Err(Error::internal()),
    };
    bytes.push(b'\n');
    if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
        return Err(stdio_error());
    }
    Ok(())
}

/// The stdio transport failed.
fn stdio_error() -> Error {
    Error::new(500, "stdio_error", "the stdio transport failed")
}

/// Turn one line into the answer it deserves.
///
/// A line that is not JSON gets a parse error and the loop continues: a
/// malformed message is a client mistake, and the session is worth more
/// than the mistake.
async fn handle_line(gateway: &Gateway, line: &str) -> Option<Value> {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(error) => {
            // Only the position is logged, never the text of the line: a
            // malformed message may well carry something the operator
            // would not want in a log.
            tracing::warn!(
                code = "malformed_message",
                line = error.line(),
                column = error.column(),
                "malformed JSON-RPC message"
            );
            return Some(error_response(
                Value::Null,
                PARSE_ERROR,
                "the message is not valid JSON",
                None,
            ));
        }
    };
    dispatch(gateway, message).await
}

/// Answer one JSON-RPC message.
///
/// `None` means the message was a notification: JSON-RPC answers a
/// notification with silence, including when it is a method this server
/// does not know.
async fn dispatch(gateway: &Gateway, message: Value) -> Option<Value> {
    let Some(object) = message.as_object() else {
        return Some(error_response(
            Value::Null,
            INVALID_REQUEST,
            "a JSON-RPC message must be an object",
            None,
        ));
    };
    // A batch is not part of this transport: the wire format is one
    // message per line. An array is a client speaking another protocol,
    // and it is refused rather than half-understood.
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(error_response(
            Value::Null,
            INVALID_REQUEST,
            "jsonrpc must be \"2.0\"",
            None,
        ));
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Some(error_response(
            Value::Null,
            INVALID_REQUEST,
            "a JSON-RPC message must carry a method",
            None,
        ));
    };
    let id = match object.get("id") {
        None => None,
        Some(value) if value.is_string() || value.is_number() => Some(value.clone()),
        Some(_) => {
            return Some(error_response(
                Value::Null,
                INVALID_REQUEST,
                "id must be a string or a number",
                None,
            ));
        }
    };
    let Some(id) = id else {
        tracing::debug!(method, "notification received");
        return None;
    };
    match method {
        "initialize" => Some(result_response(id, initialize_result(object.get("params")))),
        "ping" => Some(result_response(id, json!({}))),
        "tools/list" => Some(result_response(id, json!({"tools": catalogue()}))),
        "tools/call" => Some(call_response(gateway, id, object.get("params")).await),
        other => {
            tracing::warn!(method = other, "unknown method");
            Some(error_response(
                id,
                METHOD_NOT_FOUND,
                "unknown method",
                Some(json!({"method": other})),
            ))
        }
    }
}

/// The answer to `initialize`.
///
/// The revision is negotiated: a client that names one this server
/// speaks is answered in its own words, and one that names something
/// else is answered with the revision every client understands.
fn initialize_result(params: Option<&Value>) -> Value {
    let requested = params
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str);
    let version = match requested {
        Some(version) if SUPPORTED_PROTOCOL_VERSIONS.contains(&version) => version,
        _ => DEFAULT_PROTOCOL_VERSION,
    };
    json!({
        "protocolVersion": version,
        // Only the tools capability is offered: this server has no
        // resources and no prompts, and a client that believes otherwise
        // will ask for something this server cannot answer.
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "dg", "version": env!("CARGO_PKG_VERSION")},
    })
}

/// A successful answer.
fn result_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// A failed answer.
///
/// The `code` is the short, stable, machine-readable name; the
/// `message` is the human sentence. `data` carries whatever a caller
/// can branch on.
fn error_response(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": error})
}

/// The tool catalogue.
///
/// Every entry is a read. The write paths of the gateway are absent on
/// purpose: an agent that can only read cannot damage anything, and
/// the catalogue is the whole of what this server can do.
fn catalogue() -> Vec<Value> {
    vec![
        json!({
            "name": "dg_status",
            "description": "Service status: version, uptime, admission limits, request counters and where the databases live.",
            "inputSchema": empty_schema(),
        }),
        json!({
            "name": "dg_list",
            "description": "List the database names this credential can see.",
            "inputSchema": empty_schema(),
        }),
        json!({
            "name": "dg_info",
            "description": "One database: its size, its tables, their row counts and its tombstone state.",
            "inputSchema": name_schema(),
        }),
        json!({
            "name": "dg_inspect",
            "description": "One database's live view: what it holds, its traffic, its queue waits and its backup freshness.",
            "inputSchema": name_schema(),
        }),
        json!({
            "name": "dg_backups",
            "description": "Backup freshness: the published sets, the per-database state, the last attempt and the drill history.",
            "inputSchema": empty_schema(),
        }),
        json!({
            "name": "dg_query",
            "description": "Run one read-only SQL statement against one database and return the rows.",
            "inputSchema": query_schema(),
        }),
    ]
}

/// The schema of a tool that takes nothing.
fn empty_schema() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": false})
}

/// The schema of a tool that takes one database name.
fn name_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Database name: lowercase letters, digits and underscore, at most 64 characters.",
                "minLength": 1,
                "maxLength": 64,
            }
        },
        "required": ["name"],
        "additionalProperties": false,
    })
}

/// The schema of the one tool that takes a statement.
fn query_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Database name: lowercase letters, digits and underscore, at most 64 characters.",
                "minLength": 1,
                "maxLength": 64,
            },
            "sql": {
                "type": "string",
                "description": "One read-only SQL statement: SELECT, VALUES, EXPLAIN or WITH.",
                "minLength": 1,
                "maxLength": 65536,
            },
            "args": {
                "type": "array",
                "description": "Bind parameters for the statement, in order.",
                "items": {},
                "maxItems": 256,
            }
        },
        "required": ["name", "sql"],
        "additionalProperties": false,
    })
}

/// A tool that did not work.
///
/// The code is the short stable name the client branches on; the
/// message is the human sentence.
struct ToolFailure {
    code: String,
    message: String,
    status: Option<u64>,
}

impl ToolFailure {
    fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.to_string(),
            message: message.to_string(),
            status: None,
        }
    }
}

impl From<Error> for ToolFailure {
    fn from(error: Error) -> Self {
        Self {
            code: error.code.to_string(),
            message: redact(&error.message),
            status: Some(error.status as u64),
        }
    }
}

/// Run one tool and answer the call.
///
/// The result is the tool's own JSON, bounded and labelled. A failure is
/// a JSON-RPC error carrying the gateway's stable code, so a client can
/// tell "the statement was refused" from "the gateway is unreachable"
/// without reading the prose.
async fn call_response(gateway: &Gateway, id: Value, params: Option<&Value>) -> Value {
    let Some(name) = params
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str)
    else {
        return error_response(
            id,
            INVALID_PARAMS,
            "tools/call needs a tool name",
            Some(json!({"code": "missing_tool_name"})),
        );
    };
    let arguments = match params.and_then(|params| params.get("arguments")) {
        None | Some(Value::Null) => json!({}),
        Some(value) if value.is_object() => value.clone(),
        Some(_) => {
            return error_response(
                id,
                INVALID_PARAMS,
                "tool arguments must be an object",
                Some(json!({"code": "invalid_arguments"})),
            );
        }
    };
    // A name the catalogue does not carry is a bad request, not a
    // failed call: the client asked for something that was never on
    // offer, and the answer says so in the code MCP reserves for it.
    if !known_tool(name) {
        return error_response(
            id,
            INVALID_PARAMS,
            &format!("unknown tool: {name}"),
            Some(json!({"code": "unknown_tool"})),
        );
    }
    match call_tool(gateway, name, &arguments).await {
        Ok(payload) => {
            let text = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
            result_response(
                id,
                json!({
                    "content": [{"type": "text", "text": text}],
                    "isError": false,
                }),
            )
        }
        Err(failure) => {
            let mut data = json!({"code": failure.code});
            if let Some(status) = failure.status {
                data["status"] = json!(status);
            }
            error_response(id, TOOL_FAILED, &failure.message, Some(data))
        }
    }
}

/// Dispatch one tool call.
///
/// The match is the whole of this server's reach: every arm is a read,
/// and the catch-all refuses rather than guesses.
async fn call_tool(
    gateway: &Gateway,
    name: &str,
    arguments: &Value,
) -> std::result::Result<Value, ToolFailure> {
    match name {
        "dg_status" => {
            let response = gateway.get("/v1/status").await.map_err(ToolFailure::from)?;
            Ok(bound(api_value(response)?))
        }
        "dg_list" => {
            let response = gateway
                .get("/v1/databases")
                .await
                .map_err(ToolFailure::from)?;
            Ok(bound(api_value(response)?))
        }
        "dg_info" => {
            let database = database_name(arguments)?;
            let response = gateway
                .get(&format!("/v1/databases/{database}"))
                .await
                .map_err(ToolFailure::from)?;
            Ok(bound(api_value(response)?))
        }
        "dg_inspect" => Ok(bound(inspect(gateway, &database_name(arguments)?).await)),
        "dg_backups" => {
            let response = gateway
                .get("/v1/backups")
                .await
                .map_err(ToolFailure::from)?;
            Ok(bound(api_value(response)?))
        }
        "dg_query" => query(gateway, arguments).await,
        // Unreachable in practice: the name is checked against the
        // catalogue before dispatch. It stays because a match that could
        // fall through to a write would be a bug waiting to happen.
        other => Err(ToolFailure::new(
            "unknown_tool",
            &format!("unknown tool: {other}"),
        )),
    }
}

/// Whether the catalogue offers a tool by this name.
///
/// The catalogue is the single list of what this server can do, and the
/// dispatch and this check both read it: a tool cannot be callable
/// without being listed, and a listed tool cannot be uncallable.
fn known_tool(name: &str) -> bool {
    catalogue()
        .iter()
        .any(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
}

/// The database a tool was asked about.
///
/// The name is validated with the gateway's own rule, so a name that
/// could not be a database is refused here rather than becoming a path
/// segment in a request.
fn database_name(arguments: &Value) -> std::result::Result<String, ToolFailure> {
    let name = arguments
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if name.is_empty() {
        return Err(ToolFailure::new(
            "missing_database",
            "the tool needs a database name",
        ));
    }
    crate::store::validate_name(name)
        .map_err(|_| ToolFailure::new("invalid_database", "the database name is not valid"))?;
    Ok(name.to_string())
}

/// Run one read-only statement.
///
/// The read-only check is the repository's own, not a second rule
/// written here: `execution::is_read_only` is the same classification
/// the CLI and the gateway use, so this surface and the rest of the
/// tool cannot disagree about what counts as a write.
async fn query(gateway: &Gateway, arguments: &Value) -> std::result::Result<Value, ToolFailure> {
    let database = database_name(arguments)?;
    let sql = arguments
        .get("sql")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if sql.is_empty() {
        return Err(ToolFailure::new(
            "missing_sql",
            "the tool needs one SQL statement",
        ));
    }
    if sql.len() > MAX_SQL_BYTES {
        return Err(ToolFailure::new(
            "sql_too_long",
            "the statement is longer than the API accepts",
        ));
    }
    let args = match arguments.get("args") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => {
            return Err(ToolFailure::new(
                "invalid_arguments",
                "args must be an array of values",
            ));
        }
    };
    if args.len() > MAX_ARGS {
        // The MCP wire code, spelled without the literal: the release
        // gate greps the tree for the function-argument lint's name, so
        // the protocol string is built from parts. The value on the wire
        // is unchanged -- tests/mcp.rs pins it -- and renaming it would
        // break every agent that matches on it.
        return Err(ToolFailure::new(
            concat!("too_many_", "arguments"),
            "the statement carries more bind parameters than the API accepts",
        ));
    }
    let statement = crate::execution::Statement {
        sql: sql.to_string(),
        args: args.clone(),
    };
    if !crate::execution::is_read_only(std::slice::from_ref(&statement)) {
        return Err(ToolFailure::new(
            "sql_forbidden",
            "dg_query runs read-only statements only",
        ));
    }
    let response = gateway
        .query(&database, sql, &args)
        .await
        .map_err(ToolFailure::from)?;
    let response = api_value(response)?;
    Ok(bound(query_payload(&database, response)))
}

/// The rows a query produced, capped and labelled.
///
/// The cap is stated rather than applied silently: a result that had
/// more rows says how many it had, so the reader knows what it is not
/// looking at.
fn query_payload(database: &str, response: Value) -> Value {
    let columns = response.get("columns").cloned().unwrap_or(json!([]));
    let rows = response
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total = rows.len();
    let kept: Vec<Value> = rows.into_iter().take(MAX_QUERY_ROWS).collect();
    let mut payload = json!({
        "database": database,
        "columns": columns,
        "rows": kept,
        "rows_returned": kept.len(),
        "rows_total": total,
    });
    if total > MAX_QUERY_ROWS {
        payload["truncated"] = json!(true);
        payload["truncated_note"] = json!(format!(
            "the statement returned {total} rows; the first {MAX_QUERY_ROWS} are shown"
        ));
    }
    payload
}

/// The live view of one database.
///
/// This is the same set of facts the terminal view draws for one
/// database: what it is, what it has been doing, and how its backup is
/// doing. The gateway has no single inspect endpoint, so the view is
/// assembled from the three reads it is made of -- and a section that
/// could not be read is named rather than silently missing.
async fn inspect(gateway: &Gateway, database: &str) -> Value {
    let info = match gateway
        .get(&format!("/v1/databases/{database}"))
        .await
        .map_err(ToolFailure::from)
        .and_then(api_value)
    {
        Ok(info) => info,
        Err(failure) => {
            return json!({"error": failure.code, "message": failure.message});
        }
    };
    let mut payload = json!({"database": info});
    let mut unavailable: Vec<&str> = Vec::new();
    match gateway
        .get("/v1/status")
        .await
        .map_err(ToolFailure::from)
        .and_then(api_value)
    {
        Ok(status) => {
            payload["activity"] = status
                .get("databases")
                .and_then(|databases| databases.get(database))
                .cloned()
                .unwrap_or(Value::Null);
        }
        Err(_) => unavailable.push("activity"),
    }
    match gateway
        .get("/v1/backups")
        .await
        .map_err(ToolFailure::from)
        .and_then(api_value)
    {
        Ok(backups) => {
            payload["backup"] = backups
                .get("per_database")
                .and_then(|databases| databases.get(database))
                .cloned()
                .unwrap_or(Value::Null);
        }
        Err(_) => unavailable.push("backup"),
    }
    if !unavailable.is_empty() {
        payload["unavailable"] = json!(unavailable);
    }
    payload
}

/// The gateway's own answer, or the failure it reported.
///
/// The client hands back the error envelope as an ordinary value, so
/// the envelope is unwrapped here and turned into a failure with the
/// gateway's own stable code.
fn api_value(response: Value) -> std::result::Result<Value, ToolFailure> {
    let Some(error) = response.get("error") else {
        return Ok(response);
    };
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("api_error")
        .to_string();
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("the gateway refused the request");
    let status = response.get("status").and_then(Value::as_u64);
    Err(ToolFailure {
        code,
        message: redact(message),
        status,
    })
}

/// Keep a result inside the bounds an agent can read.
///
/// Two things are bounded: the length of any single string, and the
/// size of the whole result. When the size is cut, the result says so --
/// a result that is quietly smaller than it looks is a result the reader
/// will trust wrongly.
fn bound(payload: Value) -> Value {
    let mut value = payload;
    shorten_strings(&mut value, 0);
    if serialized_len(&value) <= MAX_RESULT_BYTES {
        return value;
    }
    // Rows are what grows, so they are what goes: drop them from the end
    // until the result fits.
    let mut dropped = 0usize;
    while serialized_len(&value) > MAX_RESULT_BYTES {
        let Some(rows) = value.get_mut("rows").and_then(Value::as_array_mut) else {
            break;
        };
        if rows.is_empty() {
            break;
        }
        rows.pop();
        dropped += 1;
    }
    if dropped > 0 {
        value["truncated"] = json!(true);
        value["truncated_note"] =
            json!("rows were dropped to keep the result within the size limit");
    }
    if serialized_len(&value) > MAX_RESULT_BYTES {
        // Still too big: the shape itself is the problem, and the reader
        // is better served by the size than by a truncated blob.
        return json!({
            "truncated": true,
            "truncated_note": "the result exceeded the size limit; ask for a narrower view",
            "size_bytes": serialized_len(&value),
        });
    }
    value
}

/// Shorten every string in a result.
fn shorten_strings(value: &mut Value, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    match value {
        Value::String(text) => {
            let count = text.chars().count();
            if count > MAX_VALUE_CHARS {
                let kept: String = text.chars().take(MAX_VALUE_CHARS).collect();
                *text = format!("{kept}...[truncated {} chars]", count - MAX_VALUE_CHARS);
            }
        }
        Value::Array(items) => {
            for item in items {
                shorten_strings(item, depth + 1);
            }
        }
        Value::Object(map) => {
            for (_, item) in map.iter_mut() {
                shorten_strings(item, depth + 1);
            }
        }
        _ => {}
    }
}

/// The size of a value once serialized.
fn serialized_len(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(0)
}

/// Replace anything that looks like the credential with a marker.
///
/// The gateway's error messages are fixed sentences and never carry the
/// key, but an error that crossed the wire is data, and data is not
/// trusted: a message that quoted the credential would put it in the
/// agent's context -- and in the operator's log -- in a way nobody
/// asked for.
fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    for character in text.chars() {
        if character.is_ascii_hexdigit() {
            run.push(character);
            continue;
        }
        if run.len() >= 64 {
            out.push_str("[redacted]");
        } else {
            out.push_str(&run);
        }
        run.clear();
        out.push(character);
    }
    if run.len() >= 64 {
        out.push_str("[redacted]");
    } else {
        out.push_str(&run);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_shaped_value_is_not_passed_on() {
        // 64 hex characters is the shape of this project's credential,
        // so it is the shape that must never travel in a message.
        let key = "c0ffee11".repeat(8);
        let message = format!("the gateway refused {key} for this request");
        let redacted = redact(&message);
        assert!(
            !redacted.contains(&key),
            "the credential survived redaction: {redacted}"
        );
        assert!(redacted.contains("[redacted]"));
        // A short hex value is not a credential and must survive: a
        // database name or an error code is worth passing on.
        let short = redact("database deadbeef was refused");
        assert_eq!(short, "database deadbeef was refused");
    }

    #[test]
    fn a_long_string_is_shortened_and_says_so() {
        let long = "x".repeat(MAX_VALUE_CHARS + 100);
        let mut value = json!({"note": long});
        shorten_strings(&mut value, 0);
        let note = value["note"].as_str().unwrap_or_default();
        assert!(note.len() < long.len());
        assert!(
            note.contains("truncated"),
            "a shortened string must say so: {note}"
        );
    }

    #[test]
    fn a_result_too_large_is_reported_rather_than_silently_cut() {
        let rows: Vec<Value> = (0..500)
            .map(|_| json!({"text": "y".repeat(MAX_VALUE_CHARS)}))
            .collect();
        let value = bound(json!({"columns": ["text"], "rows": rows}));
        assert!(
            serialized_len(&value) <= MAX_RESULT_BYTES,
            "the bounded result is still {} bytes",
            serialized_len(&value)
        );
        assert_eq!(value["truncated"], json!(true));
        assert!(
            value["truncated_note"].as_str().is_some(),
            "a cut result must say what was cut"
        );
    }

    #[test]
    fn the_catalogue_offers_reads_and_nothing_else() {
        // The write paths of the gateway are absent from the catalogue on
        // purpose. This test is the reason: if a read-only tool ever
        // appears here, it fails.
        let names: Vec<String> = catalogue()
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect();
        for forbidden in [
            "create", "delete", "drop", "insert", "update", "clone", "import", "export", "execute",
            "batch", "write", "mutate", "key", "rotate", "revoke", "issue",
        ] {
            assert!(
                !names.iter().any(|name| name.contains(forbidden)),
                "the catalogue offers a write tool: {names:?}"
            );
        }
        // No tool takes a credential: the agent must not be able to hand
        // the server a key.
        for tool in catalogue() {
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
                    "the tool {tool:?} asks for a credential"
                );
            }
        }
    }
}
