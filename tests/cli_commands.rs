//! Contract tests for the new commands: `dg key tokens`, `dg execute`
//! and `dg inspect`.
//!
//! `dg key tokens` answers "who can touch this database", which is the
//! first question of a leaked-key incident. `dg execute` runs a SQL
//! file, and the file's statements have to be found correctly -- a
//! semicolon inside a string is not a statement boundary.
//!
//! Run: `timeout 300 cargo test --locked --test cli_commands`

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use std::process::Command;

/// Strip ANSI escapes so a text assertion sees the words.
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

/// Run a command under a pty so the CLI takes its terminal path, and
/// return what it drew. The exit code comes back too: a command that
/// fails still draws its view, and that view is the point.
fn pty(fleet: &Fleet, args: &str) -> (bool, String) {
    pty_with_key(fleet, fleet.cli.key_file.to_str().unwrap(), args)
}

/// The same, under a different credential: the key file is a parameter
/// because a view that only the owner can reach is not the only view
/// worth testing.
fn pty_with_key(fleet: &Fleet, key_file: &str, args: &str) -> (bool, String) {
    let out = Command::new("script")
        .args([
            "-qec",
            &format!(
                "{} --server {} --key-file {} {args}",
                env!("CARGO_BIN_EXE_dg"),
                fleet.cli.server,
                key_file,
            ),
            "/dev/null",
        ])
        .output()
        .unwrap();
    (
        out.status.success(),
        plain(&String::from_utf8_lossy(&out.stdout)),
    )
}

/// Run a command and return its stdout, whatever the exit code.
fn run(fleet: &Fleet, args: &[&str]) -> (bool, String, String) {
    fleet.cli.run(args)
}

#[test]
fn key_tokens_lists_the_credentials_that_can_reach_a_database() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "reached_db"]);
    fleet.cli.ok("create", &["create", "other_db"]);
    fleet.cli.ok(
        "issue",
        &[
            "key",
            "create",
            "bot",
            "--role",
            "database_read_only",
            "--database",
            "reached_db",
        ],
    );

    let (_ok, screen) = pty(&fleet, "key tokens reached_db");
    // The scoped key reaches the database it was scoped to.
    assert!(
        screen.contains("bot"),
        "the scoped key is missing: {screen}"
    );
    assert!(
        screen.contains("reached_db"),
        "the scope is missing: {screen}"
    );
    // The owner key reaches everything.
    assert!(
        screen.contains("owner"),
        "the owner key is missing: {screen}"
    );

    // A key scoped to another database does NOT reach this one.
    let (_ok, screen) = pty(&fleet, "key tokens other_db");
    assert!(
        !screen.contains("bot"),
        "the scoped key reached the wrong database: {screen}"
    );
    assert!(
        screen.contains("owner"),
        "the owner key is missing: {screen}"
    );
}

#[test]
fn a_single_statement_error_envelope_is_unchanged() {
    // A single statement is unambiguous, so its error envelope keeps
    // exactly the shape it has always had. The statement index is only
    // added for a batch, where it is the thing the operator is missing.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "envelope_db"]);
    let (ok, stdout, _stderr) = run(
        &fleet,
        &["query", "envelope_db", "SELECT * FROM no_such_table"],
    );
    assert!(!ok, "the query should have failed");
    let reported: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("not JSON: {stdout}"));
    assert_eq!(reported["error"]["code"], "invalid_sql", "{stdout}");
    assert_eq!(reported["error"]["message"], "SQL or parameters invalid");
    assert!(
        reported["error"].get("statement").is_none(),
        "a single statement gained a statement index: {stdout}"
    );
}

#[test]
fn key_tokens_narrows_the_answer_for_a_script_too() {
    // The answer is narrowed for BOTH forms. A script that asks which
    // credentials can reach a database must get the credentials that
    // can -- not every credential the gateway holds, which is what the
    // raw endpoint answers.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "reached_db"]);
    fleet.cli.ok("create", &["create", "other_db"]);
    fleet.cli.ok(
        "issue",
        &[
            "key",
            "create",
            "bot",
            "--role",
            "database_read_only",
            "--database",
            "reached_db",
        ],
    );

    for database in ["reached_db", "other_db"] {
        let (ok, stdout, stderr) = fleet.cli.run(&["--json", "key", "tokens", database]);
        assert!(ok, "failed: {stderr}");
        let answer: serde_json::Value =
            serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("not JSON: {stdout}"));
        let names: Vec<String> = answer["keys"]
            .as_array()
            .expect("keys is an array")
            .iter()
            .map(|key| key["name"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            names.contains(&"owner".to_string()),
            "the owner key is missing for {database}: {names:?}"
        );
        let scoped = names.contains(&"bot".to_string());
        assert_eq!(
            scoped,
            database == "reached_db",
            "the scoped key is in the answer for the wrong database: {names:?}"
        );
    }
}

#[test]
fn key_tokens_stays_machine_readable_when_piped() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "piped_db"]);
    let (ok, stdout, stderr) = run(&fleet, &["--json", "key", "tokens", "piped_db"]);
    assert!(ok, "failed: {stderr}");
    assert!(
        serde_json::from_str::<serde_json::Value>(&stdout).is_ok(),
        "not JSON: {stdout}"
    );
}

#[test]
fn execute_runs_every_statement_of_a_script_and_reports_each_one() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "script_db"]);

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("script.sql");
    std::fs::write(
        &script,
        // A comment, a semicolon inside a string, and a semicolon in a
        // comment: none of them is a statement boundary.
        "-- set up\n\
         CREATE TABLE players (id INTEGER PRIMARY KEY, name TEXT, note TEXT);\n\
         INSERT INTO players VALUES (1, 'aria', 'a;b');\n\
         -- a trailing comment with a semicolon ;\n\
         INSERT INTO players VALUES (2, 'octavia', 'plain');\n",
    )
    .unwrap();

    let (_ok, screen) = pty(
        &fleet,
        &format!("execute script_db {}", script.to_str().unwrap()),
    );
    // Every statement is reported, in order.
    assert!(screen.contains("CREATE TABLE"), "no create: {screen}");
    assert!(screen.contains("INSERT"), "no insert: {screen}");
    assert_eq!(
        screen.matches("INSERT").count(),
        2,
        "both inserts are missing: {screen}"
    );

    // The rows landed, and the semicolon inside the string survived as
    // data rather than splitting the statement.
    let rows = fleet.cli.q(
        "read back",
        "script_db",
        "SELECT id, name, note FROM players ORDER BY id",
    );
    assert_eq!(rows[0][2], "a;b", "the string was split: {rows:?}");
    assert_eq!(rows[1][2], "plain", "the string was split: {rows:?}");
    assert_eq!(rows[1][1], "octavia");
}

#[test]
fn execute_refuses_a_script_with_no_statements() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "empty_db"]);
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("empty.sql");
    std::fs::write(&script, "-- nothing but a comment\n").unwrap();
    let (ok, _stdout, stderr) = run(&fleet, &["execute", "empty_db", script.to_str().unwrap()]);
    assert!(!ok, "an empty script was accepted");
    assert!(
        stderr.contains("empty_script") || stderr.contains("no statements"),
        "the refusal does not say why: {stderr}"
    );
}

#[test]
fn execute_reports_a_failing_statement() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "failing_db"]);
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("bad.sql");
    // The second statement cannot run: the table does not exist.
    std::fs::write(
        &script,
        "SELECT * FROM no_such_table;\nINSERT INTO other VALUES (1);\n",
    )
    .unwrap();
    let (ok, stdout, stderr) = run(&fleet, &["execute", "failing_db", script.to_str().unwrap()]);
    assert!(!ok, "a failing script was accepted: {stdout}");
    // The failure names the statement that broke the script, so the
    // operator does not have to bisect it by hand. The error itself is
    // the gateway's stable one: it does not leak the table name.
    let reported: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("the failure is not JSON: {stdout}"));
    assert_eq!(
        reported["error"]["statement"], 0,
        "the failing statement is not identified: {stdout}"
    );
    assert_eq!(reported["error"]["code"], "invalid_sql", "{stdout}");
    assert!(stderr.contains("api_error"), "no error on stderr: {stderr}");
}

#[test]
fn a_failing_script_names_the_statement_that_broke_it_on_a_terminal() {
    // On a terminal the failure is drawn with the statement it came
    // from, not as a bare envelope.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "failing_pty_db"]);
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("bad.sql");
    std::fs::write(
        &script,
        "SELECT 1;\nSELECT * FROM no_such_table;\nSELECT 2;\n",
    )
    .unwrap();
    let (ok, screen) = pty(
        &fleet,
        &format!("execute failing_pty_db {}", script.to_str().unwrap()),
    );
    // The script failed, and the view says which statement broke it.
    assert!(!ok, "the failing script exited zero: {screen}");
    // Every statement is listed, and the one that failed is marked.
    assert_eq!(screen.matches("SELECT").count(), 3, "{screen}");
    assert!(
        screen.contains("rolled back"),
        "the statements that did not run are not marked: {screen}"
    );
    assert!(
        screen.contains("invalid_sql"),
        "the failure is not shown: {screen}"
    );
}

#[test]
fn a_one_statement_script_that_fails_shows_why_and_is_not_called_not_run() {
    // The server attributes an error to a statement only when the batch
    // holds more than one, because a lone statement is unambiguous on
    // its own. So a script of one statement that fails carries no
    // index, and that absence says nothing about whether it ran: the
    // statement did run, and it failed. The row must carry the reason,
    // not claim the statement never ran.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "lone_db"]);
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("lone.sql");
    std::fs::write(&script, "SELECT * FROM no_such_table;\n").unwrap();
    let (ok, screen) = pty(
        &fleet,
        &format!("execute lone_db {}", script.to_str().unwrap()),
    );
    assert!(!ok, "the failing script exited zero: {screen}");
    assert!(
        !screen.contains("not run"),
        "a statement that ran and failed is drawn as not run: {screen}"
    );
    assert!(
        screen.contains("invalid_sql"),
        "the reason the statement failed is not shown: {screen}"
    );
}

#[test]
fn execute_stays_machine_readable_when_piped() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "piped_script_db"]);
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("script.sql");
    std::fs::write(&script, "CREATE TABLE t (id INTEGER);\n").unwrap();
    let (ok, stdout, stderr) = run(
        &fleet,
        &["execute", "piped_script_db", script.to_str().unwrap()],
    );
    assert!(ok, "failed: {stderr}");
    assert!(
        serde_json::from_str::<serde_json::Value>(&stdout).is_ok(),
        "not JSON: {stdout}"
    );
}

#[test]
fn execute_refuses_an_over_long_script_before_sending_it() {
    // The batch API takes 32 statements; the whole file is one atomic
    // batch, so a longer file cannot be split without giving that up.
    // The CLI knows the cap, so it refuses here — with the count, the
    // cap and the fact that nothing was sent — instead of sending a
    // request the server is certain to reject with a bare error.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "long_script_db"]);
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("long.sql");
    let mut body = String::from("CREATE TABLE landed (id INTEGER);\n");
    for index in 0..32 {
        body.push_str(&format!("INSERT INTO landed VALUES ({index});\n"));
    }
    std::fs::write(&script, &body).unwrap();

    let (ok, _stdout, stderr) = run(
        &fleet,
        &["execute", "long_script_db", script.to_str().unwrap()],
    );
    assert!(!ok, "an over-long script was accepted");
    assert!(
        stderr.contains("too_many_statements"),
        "the refusal does not name the reason: {stderr}"
    );
    // The message is the operator's instruction: how many the file
    // holds, what the API accepts, and that nothing was sent.
    assert!(
        stderr.contains("33 statements"),
        "the refusal does not say how many the file holds: {stderr}"
    );
    assert!(
        stderr.contains("accepts 32"),
        "the refusal does not say what the API accepts: {stderr}"
    );
    assert!(
        stderr.contains("nothing was sent"),
        "the refusal does not say that nothing was sent: {stderr}"
    );

    // Nothing ran: the table the script would have created is not
    // there. A refusal that left a half-applied script behind would be
    // worse than the server's rejection.
    let rows = fleet.cli.q(
        "check",
        "long_script_db",
        "SELECT name FROM sqlite_master WHERE name = 'landed'",
    );
    assert!(
        rows.as_array().is_some_and(|rows| rows.is_empty()),
        "the refused script left something behind: {rows:?}"
    );
}

#[test]
fn execute_runs_a_script_of_exactly_the_cap() {
    // The cap is a boundary, not a discount: 32 statements is a legal
    // batch, and the check must not refuse one.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "cap_script_db"]);
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("cap.sql");
    let mut body = String::from("CREATE TABLE capped (id INTEGER);\n");
    for index in 0..31 {
        body.push_str(&format!("INSERT INTO capped VALUES ({index});\n"));
    }
    std::fs::write(&script, &body).unwrap();

    let (ok, _stdout, stderr) = run(
        &fleet,
        &["execute", "cap_script_db", script.to_str().unwrap()],
    );
    assert!(ok, "a script of exactly the cap was refused: {stderr}");
    let rows = fleet
        .cli
        .q("count", "cap_script_db", "SELECT count(*) FROM capped");
    assert_eq!(rows[0][0], 31, "not every statement ran: {rows:?}");
}

#[test]
fn execute_does_not_claim_a_rollback_when_the_batch_never_ran() {
    // A rejection that names no statement is a rejection of the whole
    // request: the statements never ran, so nothing was rolled back.
    // The view must not claim a rollback that did not happen.
    //
    // A read-only credential with an idempotency key is such a
    // rejection: the server refuses the key on a read before it runs
    // anything.
    let fleet = Fleet::boot();
    fleet.cli.ok("create", &["create", "never_ran_db"]);
    let reader = fleet.cli.ok(
        "issue",
        &[
            "key",
            "create",
            "reader",
            "--role",
            "database_read_only",
            "--database",
            "never_ran_db",
        ],
    );
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("reader.key");
    std::fs::write(&key_file, reader["key"].as_str().unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let script = dir.path().join("reads.sql");
    std::fs::write(&script, "SELECT 1;\nSELECT 2;\n").unwrap();

    let (ok, screen) = pty_with_key(
        &fleet,
        key_file.to_str().unwrap(),
        &format!(
            "execute never_ran_db {} --idempotency-key read-with-a-key",
            script.to_str().unwrap()
        ),
    );
    assert!(!ok, "the rejected batch exited zero: {screen}");
    assert!(
        screen.contains("not run"),
        "the batch is not marked as not run: {screen}"
    );
    assert!(
        !screen.contains("rolled back"),
        "the view claims a rollback that did not happen: {screen}"
    );
}
