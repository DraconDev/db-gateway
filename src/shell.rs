//! Line-oriented HTTP shell. Each SQL line is a separate transaction; no retries.
use crate::{
    client,
    error::{Error, Result},
    store::validate_name,
};
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};

const MAX_INPUT: usize = 65536;
const HELP: &str = "dot-commands:
  use <db> / .use <db>  select a database locally (never creates one)
  .tables              list user tables
  .schema [table]      show CREATE statements
  :batch [JSON]        paste a JSON batch, possibly over multiple lines; . cancels
  :key [key]           key for the next SQL/batch request; empty clears it
  .json                toggle JSON output
  .help                show help
  .quit                quit (EOF also exits)
One SQL statement per line. No hidden retries or cross-line transactions.
A pending key is consumed on the next SQL/batch attempt, including errors.";

struct Session<'a> {
    server: &'a str,
    key: &'a str,
    database: Option<String>,
    json_mode: bool,
    pending_key: Option<String>,
    batch: Option<String>,
}

/// Blocking stdin is used only by this dedicated CLI session, not the server.
/// Prompts are enabled only for a terminal in text mode, never for piped input.
pub async fn run(
    server: &str,
    key: &str,
    json_mode: bool,
    interactive: bool,
    input: impl Read,
    mut out: impl Write,
) -> Result<()> {
    let mut session = Session {
        server,
        key,
        database: None,
        json_mode,
        pending_key: None,
        batch: None,
    };
    let mut reader = std::io::BufReader::new(input);
    loop {
        if interactive && !session.json_mode {
            if session.batch.is_some() {
                write!(out, "...> ").map_err(io)?;
            } else {
                write!(
                    out,
                    "dg{}> ",
                    session
                        .database
                        .as_ref()
                        .map(|db| format!(":{db}"))
                        .unwrap_or_default()
                )
                .map_err(io)?;
            }
        }
        out.flush().map_err(io)?;
        let mut bytes = Vec::new();
        let count = reader
            .by_ref()
            .take((MAX_INPUT + 1) as u64)
            .read_until(b'\n', &mut bytes)
            .map_err(io)?;
        if count == 0 {
            if session.batch.is_some() {
                session.error(
                    Error::new(400, "invalid_request", "incomplete batch at EOF"),
                    &mut out,
                )?;
            }
            out.flush().map_err(io)?;
            return Ok(());
        }
        if bytes.len() > MAX_INPUT {
            if bytes.last() != Some(&b'\n') {
                reader.skip_until(b'\n').map_err(io)?;
            }
            session.batch = None;
            session.pending_key = None;
            session.error(
                Error::new(413, "input_limit_exceeded", "shell input exceeds 64 KiB"),
                &mut out,
            )?;
            continue;
        }
        let line = match String::from_utf8(bytes) {
            Ok(line) => line,
            Err(_) => {
                session.batch = None;
                session.pending_key = None;
                session.error(Error::invalid(), &mut out)?;
                continue;
            }
        };
        match session.execute(line.trim(), &mut out).await {
            Ok(true) => {
                out.flush().map_err(io)?;
                return Ok(());
            }
            Ok(false) => {}
            Err(error) if error.code == "shell_io_error" => return Err(error),
            Err(error) => session.error(error, &mut out)?,
        }
    }
}

impl Session<'_> {
    fn error(&self, error: Error, out: &mut dyn Write) -> Result<()> {
        render(
            &json!({"status": error.status, "error": error}),
            self.json_mode,
            out,
        )
    }

    fn notice(&self, message: &str, out: &mut dyn Write) -> Result<()> {
        if self.json_mode {
            writeln!(out, "{}", json!({"message": message})).map_err(io)
        } else {
            writeln!(out, "{message}").map_err(io)
        }
    }

    async fn execute(&mut self, line: &str, out: &mut dyn Write) -> Result<bool> {
        if line == ".quit" || line == ".exit" {
            return Ok(true);
        }
        if self.batch.is_some() {
            return self.batch_line(line, out).await;
        }
        if line.is_empty() {
            return Ok(false);
        }
        let (command, argument) = split_command(line);
        match command {
            "use" | ".use" => {
                validate_name(argument)?;
                self.database = Some(argument.to_owned());
                self.pending_key = None;
                self.notice(&format!("using {argument}"), out)?;
            }
            ".help" => self.notice(HELP, out)?,
            ".json" => {
                self.json_mode = !self.json_mode;
                self.notice(
                    if self.json_mode {
                        "json output: on"
                    } else {
                        "json output: off"
                    },
                    out,
                )?;
            }
            ":key" => {
                if !argument.is_empty()
                    && (argument.len() > 128 || !argument.bytes().all(|b| b.is_ascii_graphic()))
                {
                    self.pending_key = None;
                    return Err(Error::invalid());
                }
                self.pending_key = (!argument.is_empty()).then(|| argument.to_owned());
                self.notice(
                    if argument.is_empty() {
                        "idempotency key cleared"
                    } else {
                        "idempotency key set for next SQL/batch attempt"
                    },
                    out,
                )?;
            }
            ":batch" => {
                self.batch = Some(String::new());
                if !argument.is_empty() {
                    return self.batch_line(argument, out).await;
                }
            }
            ".tables" | ".schema" => {
                let (sql, args) = if command == ".tables" {
                    (
                        "SELECT name FROM sqlite_master WHERE type='table' AND substr(name,1,4)!='_dg_' ORDER BY name",
                        json!([]),
                    )
                } else {
                    (
                        "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND substr(name,1,4)!='_dg_' AND (?='' OR name=?) ORDER BY name",
                        json!([argument, argument]),
                    )
                };
                let response = self
                    .request("query", json!({"sql":sql,"args":args}), None)
                    .await?;
                render(&response, self.json_mode, out)?;
            }
            _ if command.starts_with(['.', ':']) => {
                return Err(Error::new(
                    400,
                    "unknown_dot_command",
                    "unknown command; try .help",
                ));
            }
            _ => {
                let key = self.pending_key.take();
                let response = self
                    .request("query", json!({"sql":line,"args":[]}), key.as_deref())
                    .await?;
                render(&response, self.json_mode, out)?;
            }
        }
        Ok(false)
    }

    async fn batch_line(&mut self, line: &str, out: &mut dyn Write) -> Result<bool> {
        let mut text = self.batch.take().expect("batch mode");
        if line == "." {
            self.pending_key = None;
            self.notice("batch cancelled", out)?;
            return Ok(false);
        }
        if text.len() + line.len() + 1 > MAX_INPUT {
            self.pending_key = None;
            return Err(Error::new(
                413,
                "input_limit_exceeded",
                "shell input exceeds 64 KiB",
            ));
        }
        text.push_str(line);
        text.push('\n');
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) if error.is_eof() => {
                self.batch = Some(text);
                return Ok(false);
            }
            Err(_) => {
                self.pending_key = None;
                return Err(Error::invalid());
            }
        };
        let key = self.pending_key.take();
        let batch: crate::execution::Batch =
            serde_json::from_value(value).map_err(|_| Error::invalid())?;
        let response = self
            .request(
                "batch",
                serde_json::to_value(batch).map_err(|_| Error::internal())?,
                key.as_deref(),
            )
            .await?;
        render(&response, self.json_mode, out)?;
        Ok(false)
    }

    async fn request(&self, route: &str, body: Value, idempotency: Option<&str>) -> Result<Value> {
        let database = self
            .database
            .as_deref()
            .ok_or_else(|| Error::new(400, "no_database_selected", "run use <db> first"))?;
        client::request(
            self.server,
            self.key,
            reqwest::Method::POST,
            &format!("/v1/databases/{database}/{route}"),
            Some(body),
            idempotency,
        )
        .await
    }
}

fn render(response: &Value, json_mode: bool, out: &mut dyn Write) -> Result<()> {
    if json_mode {
        return writeln!(out, "{response}").map_err(io);
    }
    if let Some(error) = response.get("error") {
        return writeln!(
            out,
            "error [{}] {}: {}",
            response["status"],
            error["code"].as_str().unwrap_or("unknown"),
            error["message"].as_str().unwrap_or("")
        )
        .map_err(io);
    }
    if let Some(results) = response["results"].as_array() {
        for (index, result) in results.iter().enumerate() {
            writeln!(out, "statement {}:", index + 1).map_err(io)?;
            render(result, false, out)?;
        }
        return Ok(());
    }
    if let (Some(columns), Some(rows)) =
        (response["columns"].as_array(), response["rows"].as_array())
        && !columns.is_empty()
    {
        let header: Vec<String> = columns.iter().map(cell_text).collect();
        let rows: Vec<Vec<String>> = rows
            .iter()
            .map(|row| (0..columns.len()).map(|i| cell_text(&row[i])).collect())
            .collect();
        let widths: Vec<usize> = header
            .iter()
            .enumerate()
            .map(|(i, column)| {
                rows.iter()
                    .map(|row| row[i].chars().count())
                    .fold(column.chars().count(), usize::max)
            })
            .collect();
        write_row(&header, &widths, out)?;
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
            write_row(&row, &widths, out)?;
        }
        return Ok(());
    }
    writeln!(out, "ok, {} row(s) affected", response["rows_affected"]).map_err(io)
}

fn write_row(cells: &[String], widths: &[usize], out: &mut dyn Write) -> Result<()> {
    let row = cells
        .iter()
        .zip(widths)
        .map(|(text, width)| format!("{text:<width$}"))
        .collect::<Vec<_>>()
        .join(" | ");
    writeln!(out, "{row}").map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminal_prompts_and_quit_during_batch() {
        let mut out = Vec::new();
        run("unused", "unused", false, true, b"use demo\n:batch\n.quit\nSELECT 1\n".as_slice(), &mut out).await.unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "dg> using demo\ndg:demo> ...> ");
    }

    #[tokio::test]
    async fn oversized_input_is_drained_and_session_recovers() {
        let input = format!("{}\n.bogus\n.quit\n", "x".repeat(MAX_INPUT + 8));
        let mut out = Vec::new();
        run("unused", "unused", true, false, input.as_bytes(), &mut out).await.unwrap();
        let text = String::from_utf8(out).unwrap();
        let values: Vec<Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["error"]["code"], "input_limit_exceeded");
        assert_eq!(values[1]["error"]["code"], "unknown_dot_command");
    }

    #[tokio::test]
    async fn broken_output_is_fatal_and_control_characters_are_escaped() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> { Err(std::io::ErrorKind::BrokenPipe.into()) }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let error = run("unused", "unused", false, false, b".help\n".as_slice(), Broken).await.unwrap_err();
        assert_eq!(error.code, "shell_io_error");
        assert_eq!(cell_text(&json!("a\n\u{1b}b")), "a\\n\\u{1b}b");
    }
}

fn split_command(line: &str) -> (&str, &str) {
    line.split_once(char::is_whitespace)
        .map(|(c, a)| (c, a.trim()))
        .unwrap_or((line, ""))
}

fn io(_: std::io::Error) -> Error {
    Error::new(500, "shell_io_error", "shell input/output failed")
}

fn cell_text(cell: &Value) -> String {
    match cell {
        Value::Null => "NULL".into(),
        Value::String(text) => text
            .chars()
            .flat_map(|c| {
                if c.is_control() {
                    c.escape_default().collect::<Vec<_>>()
                } else {
                    vec![c]
                }
            })
            .collect(),
        other => other.to_string(),
    }
}
