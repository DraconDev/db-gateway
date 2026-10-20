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
A pending key is consumed on the next completed SQL/batch attempt, including
API and local errors; an `outcome_unknown` transport failure retains it for an
identical replay.";

struct Session<'a> {
    server: &'a str,
    key: &'a str,
    database: Option<String>,
    json_mode: bool,
    pending_key: Option<String>,
    batch: Option<String>,
    theme: &'a crate::render::Theme,
}

/// Blocking stdin is used only by this dedicated CLI session, not the server.
/// Prompts are enabled only for a terminal in text mode, never for piped input.
pub async fn run(
    server: &str,
    key: &str,
    json_mode: bool,
    interactive: bool,
    initial_database: Option<&str>,
    input: impl Read,
    mut out: impl Write,
) -> Result<()> {
    // The shell draws to a caller-supplied stream, so it cannot ask
    // whether that stream is a terminal. The caller knows, and passes the
    // theme it wants the session drawn with.
    let theme = crate::render::Theme::for_stream(interactive);
    run_with_theme(
        server,
        key,
        json_mode,
        interactive,
        initial_database,
        input,
        out,
        &theme,
    )
    .await
}

/// The shell session, with an explicit theme.
pub async fn run_with_theme(
    server: &str,
    key: &str,
    json_mode: bool,
    interactive: bool,
    initial_database: Option<&str>,
    input: impl Read,
    out: impl Write,
    theme: &crate::render::Theme,
) -> Result<()> {
    if let Some(name) = initial_database {
        validate_name(name)?;
    }
    let mut session = Session {
        server,
        key,
        database: initial_database.map(str::to_owned),
        json_mode,
        pending_key: None,
        batch: None,
        theme,
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
                // A truncated batch is a command the shell itself rejected, so
                // the process must not exit 0 and let a script treat the
                // partial input as a successful no-op.
                session.error(
                    Error::new(400, "invalid_request", "incomplete batch at EOF"),
                    &mut out,
                )?;
                out.flush().map_err(io)?;
                return Err(Error::new(
                    400,
                    "invalid_request",
                    "incomplete batch at EOF",
                ));
            }
            out.flush().map_err(io)?;
            return Ok(());
        }
        // The ceiling is on the SQL line itself: a trailing newline is
        // transport, not content. A line of exactly 64 KiB (plus its
        // newline) must be accepted like a 64 KiB batch is, so compare the
        // content length rather than the raw read length.
        let content_len = bytes.len() - usize::from(bytes.last() == Some(&b'\n'));
        if content_len > MAX_INPUT {
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
        render_with(
            &json!({"status": error.status, "error": error}),
            self.json_mode,
            self.theme,
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
                        "SELECT name FROM sqlite_master WHERE type='table' AND lower(substr(name,1,4))!='_dg_' ORDER BY name",
                        json!([]),
                    )
                } else {
                    (
                        "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND lower(substr(name,1,4))!='_dg_' AND (?='' OR name=?) ORDER BY name",
                        json!([argument, argument]),
                    )
                };
                let response = self
                    .request("query", json!({"sql":sql,"args":args}), None)
                    .await?;
                render_with(&response, self.json_mode, self.theme, out)?;
            }
            _ if command.starts_with(['.', ':']) => {
                return Err(Error::new(
                    400,
                    "unknown_dot_command",
                    "unknown command; try .help",
                ));
            }
            _ => {
                let response = self
                    .request_attempt("query", json!({"sql":line,"args":[]}))
                    .await?;
                render_with(&response, self.json_mode, self.theme, out)?;
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
            // Restore the buffer instead of dropping it. The incomplete-JSON
            // branch below keeps what was typed so the operator can carry on
            // editing; returning here without restoring silently discarded up
            // to 64 KiB of a batch and dropped the shell back to `dg>`.
            self.batch = Some(text);
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
        // Decode locally before consuming the key. A typed validation error
        // is still a completed shell attempt and consumes the key; a request
        // transport failure is different because the caller must be able to
        // resend the identical body with the same key after outcome_unknown.
        let batch: crate::execution::Batch = match serde_json::from_value(value) {
            Ok(batch) => batch,
            Err(_) => {
                self.pending_key = None;
                return Err(Error::invalid());
            }
        };
        let body = serde_json::to_value(batch).map_err(|_| Error::internal())?;
        let response = self.request_attempt("batch", body).await?;
        render_with(&response, self.json_mode, self.theme, out)?;
        Ok(false)
    }

    async fn request_attempt(&mut self, route: &str, body: Value) -> Result<Value> {
        let key = self.pending_key.take();
        let result = self.request(route, body, key.as_deref()).await;
        if matches!(&result, Err(error) if error.code == "outcome_unknown") {
            self.pending_key = key;
        }
        result
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

/// Human-readable renderer shared by the shell and one-shot tty output.
/// `json_mode` forces raw JSON; otherwise errors print one line, column
/// results print a table, and write results print a row count.
///
/// The table is the shared renderer in [`crate::render`], so the shell
/// and the one-shot commands draw the same grid for the same rows.
pub fn render(response: &Value, json_mode: bool, out: &mut dyn Write) -> Result<()> {
    render_with(response, json_mode, &crate::render::Theme::for_stream(false), out)
}

/// Render with an explicit theme. The one-shot commands pass a theme
/// built from the real stdout, so the table is drawn for a terminal; the
/// shell passes the same theme for the session.
pub fn render_with(
    response: &Value,
    json_mode: bool,
    theme: &crate::render::Theme,
    out: &mut dyn Write,
) -> Result<()> {
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
            render_with(result, false, theme, out)?;
        }
        return Ok(());
    }
    if let (Some(columns), Some(rows)) =
        (response["columns"].as_array(), response["rows"].as_array())
        && !columns.is_empty()
    {
        // `columns` and `rows` are untrusted wire data: the shell accepts
        // any `--server` origin including plain http. The shared renderer
        // escapes control characters in a cell, so a value cannot repaint
        // the terminal or forge a row, and it renders a short row as empty
        // cells and truncates an over-long one to the header's width.
        let headers: Vec<String> = columns.iter().map(cell_text).collect();
        let values: Vec<Vec<Value>> = rows
            .iter()
            .map(|row| {
                (0..columns.len())
                    .map(|index| row.get(index).cloned().unwrap_or(Value::Null))
                    .collect()
            })
            .collect();
        let table = crate::render::Table::from_json(&headers, &values);
        table.write(out, theme)?;
        let count = values.len();
        writeln!(out, "{}", count_line(count)).map_err(io)?;
        return Ok(());
    }
    writeln!(out, "ok, {} row(s) affected", response["rows_affected"]).map_err(io)
}

/// The line under a table saying how many rows it holds.
fn count_line(count: usize) -> String {
    if count == 1 {
        "1 row".to_string()
    } else {
        format!("{count} rows")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminal_prompts_and_quit_during_batch() {
        let mut out = Vec::new();
        run(
            "unused",
            "unused",
            false,
            true,
            None,
            b"use demo\n:batch\n.quit\nSELECT 1\n".as_slice(),
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "dg> using demo\ndg:demo> ...> "
        );
    }

    #[tokio::test]
    async fn oversized_input_is_drained_and_session_recovers() {
        let input = format!("{}\n.bogus\n.quit\n", "x".repeat(MAX_INPUT + 8));
        let mut out = Vec::new();
        run(
            "unused",
            "unused",
            true,
            false,
            None,
            input.as_bytes(),
            &mut out,
        )
        .await
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let values: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["error"]["code"], "input_limit_exceeded");
        assert_eq!(values[1]["error"]["code"], "unknown_dot_command");
    }

    #[tokio::test]
    async fn short_and_long_rows_render_without_panicking() {
        // `rows` is untrusted wire data: the shell accepts any --server
        // origin, including plain http. A row narrower than `columns` used to
        // index past the end and panic the client.
        let response = serde_json::json!({
            "columns": ["a", "b", "c"],
            "rows": [["1"], ["1", "2", "3", "4"], []],
        });
        let mut out = Vec::new();
        render(&response, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "a | b | c");
        assert_eq!(lines[1], "--+---+--");
        // Short row: missing cells render empty instead of panicking.
        assert!(lines[2].starts_with('1'), "{text}");
        // Over-long row: truncated to the header width.
        assert_eq!(lines[3], "1 | 2 | 3");
        // Empty row: all cells empty, still rendered.
        assert_eq!(lines[4], "  |   |  ");
        assert_eq!(lines.len(), 5, "{text}");
    }

    #[tokio::test]
    async fn batch_over_the_input_ceiling_keeps_what_was_typed() {
        // The incomplete-JSON branch restores the buffer so the operator can
        // keep editing; the over-limit branch used to drop it, discarding up
        // to 64 KiB and dropping the shell back to `dg>`.
        let mut session = Session {
            server: "unused",
            key: "unused",
            database: None,
            json_mode: true,
            pending_key: None,
            batch: Some("x".repeat(MAX_INPUT)),
        };
        let mut out = Vec::new();
        let error = session
            .batch_line(&"y".repeat(16), &mut out)
            .await
            .unwrap_err();
        assert_eq!(error.code, "input_limit_exceeded");
        let restored = session
            .batch
            .expect("batch buffer must survive the ceiling");
        assert_eq!(restored.len(), MAX_INPUT, "buffer must be returned intact");
    }

    #[tokio::test]
    async fn broken_output_is_fatal_and_control_characters_are_escaped() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let error = run(
            "unused",
            "unused",
            false,
            false,
            None,
            b".help\n".as_slice(),
            Broken,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "shell_io_error");
        assert_eq!(cell_text(&json!("a\n\u{1b}b")), "a\\n\\u{1b}b");
    }

    #[tokio::test]
    async fn initial_database_preselects_and_use_overrides() {
        let mut out = Vec::new();
        run(
            "unused",
            "unused",
            false,
            true,
            Some("demo"),
            b"use other\n.quit\n".as_slice(),
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "dg:demo> using other\ndg:other> "
        );
    }

    #[tokio::test]
    async fn invalid_initial_database_is_rejected() {
        let mut out = Vec::new();
        let error = run(
            "unused",
            "unused",
            false,
            false,
            Some("../escape"),
            b"".as_slice(),
            &mut out,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "invalid_request");
    }
}
