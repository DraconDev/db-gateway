//! Contract tests for the CLI's pretty-print layer (the shared renderer
//! in `src/render.rs`).
//!
//! Two things are protected here, and they are the two things a reader
//! would notice if they broke:
//!
//! * **The machine surface is untouched.** Piped output and `--json`
//!   output stay byte-identical JSON, whatever the terminal does. A
//!   pretty printer that leaked decoration into a pipeline would break
//!   every script that reads the CLI.
//! * **The grid is measured, not counted.** A cell holding a CJK
//!   character or an emoji is two columns wide, and a grid that counted
//!   characters would shear. The grid has to stay square.
//!
//! Run: `timeout 300 cargo test --locked --test cli_pretty`

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use std::process::Command;

/// Strip ANSI escapes so a text assertion sees the words, not the
/// colour codes around them.
fn plain(screen: &str) -> String {
    let mut out = String::with_capacity(screen.len());
    let mut chars = screen.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
        }
        for c in chars.by_ref() {
            if ('\u{40}'..='\u{7e}').contains(&c) {
                break;
            }
        }
    }
    out
}

/// Replace the live-measurement fields with null, everywhere they
/// appear, so two responses a moment apart can be compared. The shape
/// of the response is untouched.
fn mask_live(value: &mut serde_json::Value, live: &[&str]) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                if live.contains(&key.as_str()) {
                    *child = serde_json::Value::Null;
                } else {
                    mask_live(child, live);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                mask_live(item, live);
            }
        }
        _ => {}
    }
}

/// Run a command under a pty so the CLI takes its terminal path.
fn pty(base: &str, args: &str) -> String {
    let out = Command::new("script")
        .args(["-qec", &format!("{base} {args}"), "/dev/null"])
        .output()
        .unwrap();
    assert!(out.status.success(), "pty {args} failed: {out:?}");
    plain(&String::from_utf8_lossy(&out.stdout))
}

#[test]
fn piped_output_stays_json_and_identical_to_the_json_flag() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "pretty_db"]);
    fleet.cli.q(
        "seed",
        "pretty_db",
        "CREATE TABLE t (id INTEGER, name TEXT)",
    );
    fleet.cli.q(
        "insert",
        "pretty_db",
        "INSERT INTO t VALUES (1, 'ada'), (2, 'bob')",
    );

    // The piped leg is the machine surface. It must be JSON, and it must
    // be the same JSON the --json flag produces.
    for args in [
        "list",
        "info pretty_db",
        "status",
        "backup status",
        "key list",
    ] {
        let piped = Command::new(env!("CARGO_BIN_EXE_dg"))
            .args([
                "--server",
                &fleet.cli.server,
                "--key-file",
                fleet.cli.key_file.to_str().unwrap(),
            ])
            .args(args.split(' '))
            .output()
            .unwrap();
        let piped_text = String::from_utf8_lossy(&piped.stdout).to_string();
        assert!(
            serde_json::from_str::<serde_json::Value>(&piped_text).is_ok(),
            "piped `{args}` is not JSON: {piped_text}"
        );

        let forced = Command::new(env!("CARGO_BIN_EXE_dg"))
            .args([
                "--server",
                &fleet.cli.server,
                "--key-file",
                fleet.cli.key_file.to_str().unwrap(),
                "--json",
            ])
            .args(args.split(' '))
            .output()
            .unwrap();
        let forced_text = String::from_utf8_lossy(&forced.stdout).to_string();

        // Some fields are live measurements, so two calls a moment
        // apart can differ by the work the first call itself did: the
        // database's size on disk, the service's uptime, the request and
        // per-database counters. Those are masked before the values are
        // compared; the STRUCTURE is still compared exactly, so a
        // difference in shape is caught.
        let live = [
            "size_bytes",
            "uptime_secs",
            "total",
            "capacity_exceeded",
            "query_deadline_exceeded",
            "internal_error",
            "rows_read",
            "rows_written",
            "queue_waits",
            "queue_wait_ms",
            "queue_timeouts",
        ];
        let mask = |value: &mut serde_json::Value| mask_live(value, &live);
        let mut left: serde_json::Value = serde_json::from_str(&forced_text).unwrap();
        let mut right: serde_json::Value = serde_json::from_str(&piped_text).unwrap();
        mask(&mut left);
        mask(&mut right);
        assert_eq!(
            left, right,
            "--json differs from the piped default for `{args}`"
        );
        assert!(
            !piped_text.contains('\u{1b}'),
            "colour leaked into piped output for `{args}`: {piped_text}"
        );
        assert!(
            !piped_text.contains('│'),
            "table borders leaked into piped output for `{args}`: {piped_text}"
        );
    }
}

#[test]
fn the_terminal_path_draws_a_table_and_no_json() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "pretty_db"]);
    fleet.cli.q(
        "seed",
        "pretty_db",
        "CREATE TABLE t (id INTEGER, name TEXT)",
    );
    fleet.cli.q(
        "insert",
        "pretty_db",
        "INSERT INTO t VALUES (1, 'ada'), (2, 'bob')",
    );

    let base = format!(
        "{} --server {} --key-file {}",
        env!("CARGO_BIN_EXE_dg"),
        fleet.cli.server,
        fleet.cli.key_file.to_str().unwrap(),
    );

    let screen = pty(
        &base,
        "query pretty_db \"SELECT id, name FROM t ORDER BY id\"",
    );
    assert!(screen.contains("│ id │ name │"), "no header: {screen:?}");
    assert!(screen.contains("│  1 │ ada"), "no row: {screen:?}");
    assert!(screen.contains("2 rows"), "no row count: {screen:?}");
    assert!(
        !screen.contains("\"rows\""),
        "JSON leaked to tty: {screen:?}"
    );
    // The grid is square: every line of the table is the same width.
    let table: Vec<&str> = screen
        .lines()
        .filter(|line| line.contains('│') || line.contains('┌'))
        .collect();
    let widths: Vec<usize> = table.iter().map(|line| line.chars().count()).collect();
    assert!(
        widths.windows(2).all(|w| w[0] == w[1]),
        "the grid is not square: {widths:?}"
    );
}

#[test]
fn wide_characters_do_not_shear_the_grid() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "pretty_wide"]);
    fleet.cli.q(
        "seed",
        "pretty_wide",
        "CREATE TABLE t (id INTEGER, label TEXT)",
    );
    // A label that is two columns wide per character, next to a plain
    // numeric column: the two must still line up.
    fleet.cli.q(
        "insert",
        "pretty_wide",
        "INSERT INTO t VALUES (1, '日本語'), (2, 'x')",
    );

    let base = format!(
        "{} --server {} --key-file {}",
        env!("CARGO_BIN_EXE_dg"),
        fleet.cli.server,
        fleet.cli.key_file.to_str().unwrap(),
    );
    let screen = pty(
        &base,
        "query pretty_wide \"SELECT id, label FROM t ORDER BY id\"",
    );
    assert!(
        screen.contains("日本語"),
        "the wide value is missing: {screen:?}"
    );

    // Every line of the grid has the same display width, so the wide
    // cell did not push the rest of the row out of alignment.
    let widths: Vec<usize> = screen
        .lines()
        .filter(|line| line.contains('│') || line.contains('┌'))
        .map(|line| {
            use unicode_width::UnicodeWidthStr;
            line.width()
        })
        .collect();
    assert!(
        widths.windows(2).all(|w| w[0] == w[1]),
        "the grid sheared around the wide characters: {widths:?}"
    );
}

#[test]
fn a_null_value_is_shown_as_null() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "pretty_null"]);
    fleet
        .cli
        .q("seed", "pretty_null", "CREATE TABLE t (id INTEGER, v TEXT)");
    fleet
        .cli
        .q("insert", "pretty_null", "INSERT INTO t VALUES (1, NULL)");

    let base = format!(
        "{} --server {} --key-file {}",
        env!("CARGO_BIN_EXE_dg"),
        fleet.cli.server,
        fleet.cli.key_file.to_str().unwrap(),
    );
    let screen = pty(&base, "query pretty_null \"SELECT id, v FROM t\"");
    assert!(screen.contains("NULL"), "the null is not shown: {screen:?}");
}

#[test]
fn the_backup_view_shows_the_freshness_state() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "pretty_backup"]);

    let base = format!(
        "{} --server {} --key-file {}",
        env!("CARGO_BIN_EXE_dg"),
        fleet.cli.server,
        fleet.cli.key_file.to_str().unwrap(),
    );
    let screen = pty(&base, "backup status");
    // The state word is always spelled out, so the colour is emphasis
    // rather than the only rendering of the state.
    assert!(
        screen.contains("fresh") || screen.contains("stale") || screen.contains("unknown"),
        "no freshness state shown: {screen:?}"
    );
    assert!(screen.contains("root"), "no backup root shown: {screen:?}");
}

#[test]
fn colour_is_off_when_the_environment_says_so() {
    // The documented environment rules, exercised through the real
    // binary: NO_COLOR and a dumb terminal each turn colour off, and
    // the output is still the same table without it.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "pretty_colour"]);
    fleet
        .cli
        .q("seed", "pretty_colour", "CREATE TABLE t (id INTEGER)");

    let base = format!(
        "{} --server {} --key-file {}",
        env!("CARGO_BIN_EXE_dg"),
        fleet.cli.server,
        fleet.cli.key_file.to_str().unwrap(),
    );
    let inner = format!("{base} query pretty_colour \"SELECT 1 AS n\"");

    let run = |no_color: Option<&str>, term: &str| {
        let mut command = Command::new("script");
        match no_color {
            Some(value) => command.env("NO_COLOR", value),
            None => command.env_remove("NO_COLOR"),
        };
        let out = command
            .env("TERM", term)
            .args(["-qec", &inner, "/dev/null"])
            .output()
            .unwrap();
        assert!(out.status.success(), "pty failed: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // NO_COLOR set: no escape sequences, but the same table.
    let no_color = run(Some("1"), "xterm-256color");
    assert!(
        !no_color.contains('\u{1b}'),
        "colour survived NO_COLOR: {no_color:?}"
    );
    assert!(
        no_color.contains("│ n │"),
        "the table is gone: {no_color:?}"
    );

    // A dumb terminal: no escape sequences either.
    let dumb = run(None, "dumb");
    assert!(
        !dumb.contains('\u{1b}'),
        "colour survived TERM=dumb: {dumb:?}"
    );

    // Neither set: the styled table.
    let styled = run(None, "xterm-256color");
    assert!(
        styled.contains('\u{1b}'),
        "no colour in a normal terminal: {styled:?}"
    );
}

#[test]
fn the_shell_speaks_json_when_asked() {
    // The shell's JSON mode is part of the machine surface: a script
    // that pipes into `dg shell --json` gets one JSON document per
    // line, with no table and no colour.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "pretty_shell"]);

    let base = format!(
        "{} --server {} --key-file {}",
        env!("CARGO_BIN_EXE_dg"),
        fleet.cli.server,
        fleet.cli.key_file.to_str().unwrap(),
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_dg"))
        .args([
            "--server",
            &fleet.cli.server,
            "--key-file",
            fleet.cli.key_file.to_str().unwrap(),
            "--json",
            "shell",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(b"use pretty_shell\nSELECT 1 AS n\n.quit\n")
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains('\u{1b}'),
        "colour leaked into the shell's JSON mode: {text:?}"
    );
    assert!(
        !text.contains('│'),
        "a table leaked into JSON mode: {text:?}"
    );
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|_| panic!("the shell's JSON mode emitted a non-JSON line: {line:?}"));
    }
    assert!(
        text.contains("\"columns\""),
        "the shell's JSON mode did not emit the result: {text:?}"
    );
    let _ = base;
}
