//! The terminal view's actions: what each one asks the gateway for.
//!
//! The view must not invent a call the CLI would not make, and a
//! destructive act must be confirmed by the same rule the CLI uses.

use dg::tui::actions::{ActionKind, Invocation};
use reqwest::Method;

/// A gateway that is not there: nothing can arrive.
const NOWHERE: &str = "http://127.0.0.1:1";

#[test]
fn every_action_is_in_the_catalogue_once() {
    // A palette entry that is missing makes an operation unreachable;
    // a duplicate makes the same operation appear twice.
    let kinds = [
        ActionKind::Refresh,
        ActionKind::Query,
        ActionKind::Tables,
        ActionKind::OpenTable,
        ActionKind::ExecuteFile,
        ActionKind::BatchFile,
        ActionKind::CreateDatabase,
        ActionKind::DeleteDatabase,
        ActionKind::CloneDatabase,
        ActionKind::ExportDatabase,
        ActionKind::ImportDatabase,
        ActionKind::IssueKey,
        ActionKind::RevokeKey,
        ActionKind::RotateOwner,
        ActionKind::Quit,
    ];
    for kind in kinds {
        let matches = dg::tui::actions::CATALOG
            .iter()
            .filter(|action| action.kind == kind)
            .count();
        assert_eq!(matches, 1, "{kind:?} is in the catalogue {matches} times");
    }
    assert_eq!(
        dg::tui::actions::CATALOG.len(),
        kinds.len(),
        "the catalogue holds an action that is not on the list"
    );
}

#[test]
fn the_actions_ask_the_gateway_for_what_the_api_documents() {
    // Each action's method and path are the API's own, so the view
    // cannot drift from the contract the CLI and the TypeScript client
    // also use.
    let cases: Vec<(ActionKind, &str, Method, &str)> = vec![
        (
            ActionKind::Query,
            "demo select 1",
            Method::POST,
            "/v1/databases/demo/query",
        ),
        (
            ActionKind::Tables,
            "demo",
            Method::GET,
            "/v1/databases/demo",
        ),
        (
            ActionKind::OpenTable,
            "demo products",
            Method::POST,
            "/v1/databases/demo/query",
        ),
        (
            ActionKind::DeleteDatabase,
            "demo",
            Method::DELETE,
            "/v1/databases/demo",
        ),
        (
            ActionKind::CloneDatabase,
            "demo copy",
            Method::POST,
            "/v1/databases/demo/clone",
        ),
        (
            ActionKind::ExportDatabase,
            "demo /tmp/demo.sqlite",
            Method::POST,
            "/v1/databases/demo/export/begin",
        ),
        (
            ActionKind::ImportDatabase,
            "demo /tmp/demo.sqlite",
            Method::POST,
            "/v1/databases/demo/import/begin",
        ),
        (
            ActionKind::IssueKey,
            "robot database_read_only demo",
            Method::POST,
            "/v1/keys",
        ),
        (
            ActionKind::RevokeKey,
            "robot",
            Method::DELETE,
            "/v1/keys/robot",
        ),
        (
            ActionKind::RotateOwner,
            "",
            Method::POST,
            "/v1/keys/owner/rotate",
        ),
    ];
    for (kind, arguments, method, path) in cases {
        let invocation = kind.parse(arguments).expect("the arguments parse");
        if matches!(
            invocation,
            Invocation::ExportDatabase { .. } | Invocation::ImportDatabase { .. }
        ) {
            // The two transfers are several requests, and the library
            // owns the whole sequence: they are not one request.
            assert!(
                invocation.request().is_err(),
                "{kind:?} claims to be one request"
            );
            continue;
        }
        let request = invocation
            .request()
            .expect("the invocation makes a request");
        assert_eq!(request.method, method, "{kind:?} uses the wrong method");
        assert_eq!(request.path, path, "{kind:?} uses the wrong path");
    }
}

#[test]
fn a_destructive_action_has_to_be_confirmed_by_name() {
    // The CLI's rule: the operator repeats the exact name. The view's
    // rule is the same one, so an act that destroys something cannot
    // run on a stray keypress in either surface.
    for (kind, arguments, phrase) in [
        (ActionKind::DeleteDatabase, "demo", "demo"),
        (ActionKind::RevokeKey, "robot", "robot"),
    ] {
        let action = dg::tui::actions::action(kind);
        assert!(action.confirmation, "{kind:?} is not confirmed");
        let invocation = kind.parse(arguments).expect("the arguments parse");
        assert_eq!(
            invocation.confirmation().as_deref(),
            Some(phrase),
            "{kind:?} asks for a different confirmation"
        );
        // The shared rule: the phrase has to be exact.
        assert!(
            dg::confirmation::confirm_matches(Some(phrase), phrase).is_ok(),
            "{kind:?} refuses the exact name"
        );
        assert!(
            dg::confirmation::confirm_matches(Some("Demo"), phrase).is_err(),
            "{kind:?} accepts a name that is not the name"
        );
        assert!(
            dg::confirmation::confirm_matches(None, phrase).is_err(),
            "{kind:?} accepts no confirmation at all"
        );
    }
    let rotate = ActionKind::RotateOwner.parse("").expect("no arguments");
    assert_eq!(
        rotate.confirmation().as_deref(),
        Some("rotate"),
        "the owner key is confirmed by a word"
    );
}

#[test]
fn a_whole_request_is_never_made_for_an_over_long_script() {
    // The batch API takes at most 32 statements, and a longer script
    // cannot be split without giving up the atomicity a migration
    // needs. So it is refused here, before anything is sent, and the
    // refusal says how many the file holds and what the API accepts.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("script.sql");
    let script = (0..33)
        .map(|index| format!("select {index};"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&file, script).unwrap();
    let invocation = ActionKind::ExecuteFile
        .parse(&format!("demo {}", file.display()))
        .expect("the arguments parse");
    let error = invocation
        .request()
        .expect_err("a script of 33 statements was sent");
    assert_eq!(error.code, "too_many_statements");
    let message = error.message.to_string();
    assert!(
        message.contains("33 statements"),
        "the refusal does not say how many the file holds: {message}"
    );
    assert!(
        message.contains("accepts 32"),
        "the refusal does not say what the API accepts: {message}"
    );
    assert!(
        message.contains("nothing was sent"),
        "the refusal does not say that nothing was sent: {message}"
    );
}

#[test]
fn a_batch_file_is_read_by_the_rules_the_api_sets() {
    // The same file rules as the CLI: a regular file, at most 64 KiB,
    // holding the API's own batch shape.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("batch.json");
    std::fs::write(
        &file,
        br#"{"statements":[{"sql":"select 1"}],"atomic":true}"#,
    )
    .unwrap();
    let invocation = ActionKind::BatchFile
        .parse(&format!("demo {}", file.display()))
        .expect("the arguments parse");
    let request = invocation.request().expect("the batch file parses");
    assert_eq!(request.method, Method::POST);
    assert_eq!(request.path, "/v1/databases/demo/batch");
    assert_eq!(
        request.body.expect("the batch has a body")["statements"]
            .as_array()
            .expect("the statements are a list")
            .len(),
        1
    );

    // A file that is not a batch is refused before anything is sent.
    let broken = dir.path().join("broken.json");
    std::fs::write(&broken, b"not json").unwrap();
    let invocation = ActionKind::BatchFile
        .parse(&format!("demo {}", broken.display()))
        .expect("the arguments parse");
    assert!(
        invocation.request().is_err(),
        "a file that is not a batch was sent"
    );
}

#[test]
fn the_palette_offers_the_operations_an_operator_needs() {
    // The view is the operator's surface, so the operations the CLI
    // offers are here: the lifecycle, the credentials, and the ways to
    // run SQL.
    let titles: Vec<&str> = dg::tui::actions::CATALOG
        .iter()
        .map(|action| action.title)
        .collect();
    for expected in [
        "refresh",
        "query",
        "tables",
        "open table",
        "run sql file",
        "run batch file",
        "create database",
        "delete database",
        "clone database",
        "export database",
        "import database",
        "issue key",
        "revoke key",
        "rotate owner key",
        "quit",
    ] {
        assert!(
            titles.contains(&expected),
            "the palette does not offer {expected:?}: {titles:?}"
        );
    }
    // The reading of an action's needs is part of the catalogue: an
    // action that destroys something is marked as such.
    for action in dg::tui::actions::CATALOG {
        if action.destructive {
            assert!(
                action.confirmation,
                "{} destroys something and is not confirmed",
                action.title
            );
        }
    }
}

#[test]
fn every_request_carries_what_it_can_commit() {
    // The HTTP method cannot say: a query is a POST whether it reads or
    // writes. So the request states what it can commit, with the same
    // rule the server uses and the same rule the CLI uses. A request
    // that claims to be a read when it writes would tell an operator
    // that nothing could have committed after something did.
    let read = ActionKind::Query
        .parse("demo select 1")
        .expect("the arguments parse")
        .request()
        .expect("the query makes a request");
    assert_eq!(
        read.semantics,
        dg::client::Semantics::Read,
        "a SELECT is a read"
    );

    let write = ActionKind::Query
        .parse("demo insert into t values (1)")
        .expect("the arguments parse")
        .request()
        .expect("the query makes a request");
    assert_eq!(
        write.semantics,
        dg::client::Semantics::Write,
        "an INSERT is a write"
    );

    // A batch is a read only when every statement in it is one.
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("reads.sql");
    std::fs::write(&script, "select 1;\nselect 2;\n").unwrap();
    let reads = ActionKind::ExecuteFile
        .parse(&format!("demo {}", script.display()))
        .expect("the arguments parse")
        .request()
        .expect("the script makes a request");
    assert_eq!(
        reads.semantics,
        dg::client::Semantics::Read,
        "a script of reads is a read"
    );

    let script = dir.path().join("writes.sql");
    std::fs::write(&script, "select 1;\ninsert into t values (1);\n").unwrap();
    let mixed = ActionKind::ExecuteFile
        .parse(&format!("demo {}", script.display()))
        .expect("the arguments parse")
        .request()
        .expect("the script makes a request");
    assert_eq!(
        mixed.semantics,
        dg::client::Semantics::Write,
        "a script that writes is a write"
    );

    // The lifecycle writes, whatever their method says.
    for (kind, arguments) in [
        (ActionKind::CreateDatabase, "demo"),
        (ActionKind::DeleteDatabase, "demo"),
        (ActionKind::CloneDatabase, "demo copy"),
        (ActionKind::IssueKey, "robot database_read_only demo"),
        (ActionKind::RevokeKey, "robot"),
        (ActionKind::RotateOwner, ""),
    ] {
        let request = kind
            .parse(arguments)
            .expect("the arguments parse")
            .request()
            .expect("the action makes a request");
        assert_eq!(
            request.semantics,
            dg::client::Semantics::Write,
            "{kind:?} is a write"
        );
    }

    // And the rule holds for every action that makes a request: the
    // classification is the one its own body implies, so an action
    // added later cannot claim a different one.
    for (kind, arguments) in [
        (ActionKind::Query, "demo select 1"),
        (ActionKind::Query, "demo insert into t values (1)"),
        (ActionKind::CreateDatabase, "demo"),
        (ActionKind::DeleteDatabase, "demo"),
        (ActionKind::CloneDatabase, "demo copy"),
        (ActionKind::IssueKey, "robot database_read_only demo"),
        (ActionKind::RevokeKey, "robot"),
        (ActionKind::RotateOwner, ""),
    ] {
        let request = kind
            .parse(arguments)
            .expect("the arguments parse")
            .request()
            .expect("the action makes a request");
        let body = request.body.clone().unwrap_or_default();
        let statements: Vec<String> = body["statements"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|statement| statement["sql"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let expected = if !statements.is_empty() {
            dg::client::semantics_of(&statements)
        } else if let Some(sql) = body["sql"].as_str() {
            dg::client::semantics_of(&[sql])
        } else {
            dg::client::Semantics::Write
        };
        assert_eq!(
            request.semantics, expected,
            "{kind:?} ({arguments}) is classified as something other than what it carries"
        );
    }
}

#[tokio::test]
async fn a_read_only_query_that_cannot_reach_the_gateway_says_so() {
    // The gateway is not there, so nothing was sent and nothing could
    // have committed. The view must say the gateway is unavailable. A
    // "writes may have committed" on a SELECT is advice about a write
    // that never happened.
    let mut app = dg::tui::App::new(NOWHERE.to_string(), "owner".to_string());
    app.run_invocation(Invocation::Query {
        name: "demo".to_string(),
        sql: "select 1".to_string(),
    })
    .await;
    let outcome = app.outcome.expect("the view reports what happened");
    let error = outcome.error.expect("the outcome says why it failed");
    assert!(
        error.contains("the gateway is unavailable"),
        "a read that cannot reach the gateway is an unavailable gateway: {error}"
    );
    assert!(
        !error.contains("may have committed"),
        "a read is reported as a write: {error}"
    );
}

#[tokio::test]
async fn a_query_that_writes_is_answered_as_a_write() {
    // The same view, the same unreachable gateway, and a statement that
    // writes: here the outcome really is in doubt, and the view has to
    // say so. The rule is the statements, not the method — a query is a
    // POST either way.
    let mut app = dg::tui::App::new(NOWHERE.to_string(), "owner".to_string());
    app.run_invocation(Invocation::Query {
        name: "demo".to_string(),
        sql: "insert into t values (1)".to_string(),
    })
    .await;
    let outcome = app.outcome.expect("the view reports what happened");
    let error = outcome.error.expect("the outcome says why it failed");
    assert!(
        error.contains("may have committed"),
        "a write that cannot reach the gateway leaves the outcome in doubt: {error}"
    );
}
