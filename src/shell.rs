//! Interactive REPL over the gateway HTTP API. Reads lines from any input
//! (interactive stdin or a pipe), sends each as one SQL statement, renders
//! results as an aligned table (or JSON via .json), and never retries writes.
//! Dot-commands control the session; `:batch` takes a pasted JSON body on the
//! following line; `:key` attaches an idempotency key to the next write only.
use crate::client;
use crate::error::{Error, Result};
use crate::store::validate_name;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

const HELP: &str = "dot-commands:
  .use <db>   switch the session to <db> (never creates a database)
  .tables     list tables in the current database
  .schema     show CREATE statements for the current database
  :batch      on the next line, paste one JSON batch body ({\"atomic\":true,\"statements\":[…]}) and press enter
  :key <key>  attach an idempotency key to the next write only (:key alone clears it)
  .json       toggle JSON output for results
  .help       show this help
  .quit       leave the shell
everything else is one SQL statement; a failed statement never ends the session.";

struct Session<'a> {
    server: &'a str,
    key: &'a str,
    database: Option<String>,
    json_mode: bool,
    pending_key: Option<String>,
    awaiting_batch: bool,
}

/// Read statements until EOF or `.quit`. Errors print and the loop continues.
pub async fn run(
    server: &str,
    key: &str,
    input: impl std::io::Read,
    mut out: impl Write,
) -> Result<()> {
    let mut session = Session {
        server,
        key,
        database: None,
        json_mode: false,
        pending_key: None,
        awaiting_batch: false,
    };
    for line in std::io::BufReader::new(input).lines() {
        let line = line.map_err(|_| Error::internal())?;
        if session.step(&line, &mut out).await? {
            return Ok(());
        }
        let _ = out.flush();
    }
    Ok(())
}

impl Session<'_> {
    async fn step(&mut self, line: &str, out: &mut dyn Write) -> Result<bool> {
        match self.execute(line, out).await {
            Ok(quit) => Ok(quit),
            // Stable error line; the session always survives.
            Err(error) => {
                let _ = writeln!(
                    out,
                    "error [{status}] {code}: {message}",
                    status = error.status,
                    code = error.code,
                    message = error.message
                );
                Ok(false)
            }
        }
    }

    async fn execute(&mut self, line: &str, out: &mut dyn Write) -> Result<bool> {
        let trimmed = line.trim();
        if self.awaiting_batch {
            self.awaiting_batch = false;
            if trimmed == "." {
                return Ok(false);
            }
            let database = self.require_db()?;
            let batch: crate::execution::Batch =
                serde_json::from_str(trimmed).map_err(|_| Error::invalid())?;
            let response = client::request(
                self.server,
                self.key,
                reqwest::Method::POST,
                &format!("/v1/databases/{database}/batch"),
                Some(serde_json::to_value(batch).map_err(|_| Error::internal())?),
                self.pending_key.take().as_deref(),
            )
            .await?;
            self.render(&response, out)?;
            return Ok(false);
        }
        if let Some(rest) = trimmed.strip_prefix('.') {
            return self.dot(rest, out).await;
        }
        if let Some(rest) = trimmed.strip_prefix(':') {
            let (command, argument) = split_command(rest);
            match command {
                "batch" => {
                    self.require_db()?;
                    self.awaiting_batch = true;
                    writeln!(out, "paste batch JSON, then enter (a lone . aborts)").map_err(io)?;
                }
                "key" => {
                    match argument {
                        "" => {
                            self.pending_key = None;
                            writeln!(out, "idempotency key cleared").map_err(io)?;
                        }
                        value => {
                            self.pending_key = Some(value.to_string());
                            writeln!(out, "next write will use key {value}").map_err(io)?;
                        }
                    };
                }
                other => {
                    return Err(Error::new(
                        400,
                        "unknown_dot_command",
                        Box::leak(format!("unknown command :{other}; try .help").into_boxed_str()),
                    ));
                }
            }
            return Ok(false);
        }
        if trimmed.is_empty() {
            return Ok(false);
        }
        let database = self.require_db()?;
        let response = client::request(
            self.server,
            self.key,
            reqwest::Method::POST,
            &format!("/v1/databases/{database}/query"),
            Some(json!({"sql": trimmed, "args": []})),
            self.pending_key.take().as_deref(),
        )
        .await?;
        self.render(&response, out)?;
        Ok(false)
    }

    async fn dot(&mut self, rest: &str, out: &mut dyn Write) -> Result<bool> {
        let (command, argument) = split_command(rest);
        match command {
            "quit" | "exit" => return Ok(true),
            "help" => writeln!(out, "{HELP}").map_err(io)?,
            "json" => {
                self.json_mode = !self.json_mode;
                writeln!(
                    out,
                    "json output: {}",
                    if self.json_mode { "on" } else { "off" }
                )
                .map_err(io)?;
            }
            "use" => {
                validate_name(argument)?;
                self.database = Some(argument.to_string());
                writeln!(out, "using {argument}").map_err(io)?;
            }
            "tables" => {
                let database = self.require_db()?;
                let response = self
                    .query(
                        &database,
                        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE '\\_dg\\_%' ESCAPE '\\'",
                    )
                    .await?;
                self.render(&response, out)?;
            }
            "schema" => {
                let database = self.require_db()?;
                let filter = if argument.is_empty() {
                    String::new()
                } else {
                    format!(" AND name='{}'", argument.replace('\'', "''"))
                };
                let response = self
                    .query(
                        &database,
                        &format!(
                            "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND substr(name,1,4)!='_dg_'{filter}"
                        ),
                    )
                    .await?;
                self.render(&response, out)?;
            }
            other => {
                return Err(Error::new(
                    400,
                    "unknown_dot_command",
                    Box::leak(format!("unknown command .{other}; try .help").into_boxed_str()),
                ));
            }
        }
        Ok(false)
    }

    async fn query(&self, database: &str, sql: &str) -> Result<Value> {
        client::request(
            self.server,
            self.key,
            reqwest::Method::POST,
            &format!("/v1/databases/{database}/query"),
            Some(json!({"sql": sql, "args": []})),
            None,
        )
        .await
    }

    fn require_db(&self) -> Result<&str> {
        self.database
            .as_deref()
            .ok_or_else(|| Error::new(400, "no_database_selected", "run .use <db> first"))
    }

    fn render(&self, response: &Value, out: &mut dyn Write) -> Result<()> {
        if let Some(error) = response.get("error") {
            let code = error["code"].as_str().unwrap_or("unknown");
            let message = error["message"].as_str().unwrap_or("");
            let status = response.get("status").cloned().unwrap_or(json!(""));
            return writeln!(out, "error [{status}] {code}: {message}").map_err(io);
        }
        if self.json_mode {
            return writeln!(out, "{response}").map_err(io);
        }
        match (response["columns"].as_array(), response["rows"].as_array()) {
            (Some(columns), Some(rows)) if !columns.is_empty() => {
                let widths: Vec<usize> = columns
                    .iter()
                    .enumerate()
                    .map(|(i, column)| {
                        let mut width = column.as_str().map(str::len).unwrap_or(0);
                        for row in rows {
                            width = width.max(cell_width(row.get(i).unwrap_or(&Value::Null)));
                        }
                        width
                    })
                    .collect();
                let header: Vec<String> = columns
                    .iter()
                    .enumerate()
                    .map(|(i, column)| pad(column.as_str().unwrap_or(""), widths[i]))
                    .collect();
                writeln!(out, "{}", header.join(" | ")).map_err(io)?;
                writeln!(
                    out,
                    "{}",
                    widths
                        .iter()
                        .map(|w| "-".repeat(*w))
                        .collect::<Vec<_>>()
                        .join("-+-")
                )
                .map_err(io)?;
                for row in rows {
                    let cells: Vec<String> = (0..columns.len())
                        .map(|i| pad(&cell_text(row.get(i).unwrap_or(&Value::Null)), widths[i]))
                        .collect();
                    writeln!(out, "{}", cells.join(" | ")).map_err(io)?;
                }
            }
            _ => {
                if let Some(affected) = response["rows_affected"].as_u64() {
                    writeln!(out, "ok, {affected} row(s) affected").map_err(io)?;
                }
            }
        }
        Ok(())
    }
}

fn split_command(rest: &str) -> (&str, &str) {
    match rest.split_once(char::is_whitespace) {
        Some((command, argument)) => (command, argument.trim()),
        None => (rest, ""),
    }
}

fn io(_: std::io::Error) -> Error {
    Error::internal()
}

fn cell_text(cell: &Value) -> String {
    match cell {
        Value::Null => "NULL".into(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn cell_width(cell: &Value) -> usize {
    match cell {
        Value::Null => 4,
        Value::String(text) => text.len(),
        other => other.to_string().len(),
    }
}

fn pad(text: &str, width: usize) -> String {
    format!("{text:<width$}")
}
