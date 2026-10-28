//! What the terminal view can do, and the request each action makes.
//!
//! The catalogue is data, not behaviour: an action says what it is
//! called, what the operator has to type, and which HTTP request it
//! makes. Keeping that mapping in one place means the view cannot
//! invent a call the CLI would not make, and it means the mapping can
//! be checked without a terminal.

use crate::client::Semantics;
use crate::error::{Error, Result};
use reqwest::Method;
use serde_json::{Value, json};
use std::path::PathBuf;

/// One thing the operator can do from the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    /// Read the service again now.
    Refresh,
    /// Run one statement against a database.
    Query,
    /// List one database's tables with their row counts.
    Tables,
    /// Read one database's table structure: columns, types, indexes.
    Schema,
    /// Page through one table's rows, newest page first by row order.
    OpenTable,
    /// Run a SQL file as one atomic batch.
    ExecuteFile,
    /// Run a JSON batch file.
    BatchFile,
    /// Create an empty database.
    CreateDatabase,
    /// Tombstone a database.
    DeleteDatabase,
    /// Copy a database.
    CloneDatabase,
    /// Download a database to a file.
    ExportDatabase,
    /// Upload a file as a database.
    ImportDatabase,
    /// Issue a scoped credential.
    IssueKey,
    /// Revoke a credential.
    RevokeKey,
    /// Replace the owner credential.
    RotateOwner,
    /// Leave the view.
    Quit,
}

/// One entry of the palette.
#[derive(Clone, Copy, Debug)]
pub struct Action {
    /// What the action does.
    pub kind: ActionKind,
    /// What the operator reads in the list.
    pub title: &'static str,
    /// What the operator types.
    pub hint: &'static str,
    /// Whether the action takes a line of arguments.
    pub arguments: bool,
    /// Whether the action has to be confirmed before it runs.
    pub confirmation: bool,
    /// Whether the action destroys something.
    pub destructive: bool,
}

/// The palette's contents, in the order it lists them.
pub const CATALOG: &[Action] = &[
    Action {
        kind: ActionKind::Refresh,
        title: "refresh",
        hint: "read the service again now",
        arguments: false,
        confirmation: false,
        destructive: false,
    },
    Action {
        kind: ActionKind::Query,
        title: "query",
        hint: "query <database> <sql>",
        arguments: true,
        confirmation: false,
        destructive: false,
    },
    Action {
        kind: ActionKind::Tables,
        title: "tables",
        hint: "tables <database>",
        arguments: true,
        confirmation: false,
        destructive: false,
    },
    Action {
        kind: ActionKind::Schema,
        title: "schema",
        hint: "schema <database> [table]",
        arguments: true,
        confirmation: false,
        destructive: false,
    },
    Action {
        kind: ActionKind::OpenTable,
        title: "open table",
        hint: "open <database> <table> [page]",
        arguments: true,
        confirmation: false,
        destructive: false,
    },
    Action {
        kind: ActionKind::ExecuteFile,
        title: "run sql file",
        hint: "run <database> <script.sql>",
        arguments: true,
        confirmation: false,
        destructive: false,
    },
    Action {
        kind: ActionKind::BatchFile,
        title: "run batch file",
        hint: "batch <database> <batch.json>",
        arguments: true,
        confirmation: false,
        destructive: false,
    },
    Action {
        kind: ActionKind::CreateDatabase,
        title: "create database",
        hint: "create <name>",
        arguments: true,
        confirmation: false,
        destructive: false,
    },
    Action {
        kind: ActionKind::DeleteDatabase,
        title: "delete database",
        hint: "delete <name>",
        arguments: true,
        confirmation: true,
        destructive: true,
    },
    Action {
        kind: ActionKind::CloneDatabase,
        title: "clone database",
        hint: "clone <source> <destination>",
        arguments: true,
        confirmation: false,
        destructive: false,
    },
    Action {
        kind: ActionKind::ExportDatabase,
        title: "export database",
        hint: "export <name> <file.sqlite>",
        arguments: true,
        confirmation: false,
        destructive: false,
    },
    Action {
        kind: ActionKind::ImportDatabase,
        title: "import database",
        hint: "import <name> <file.sqlite>",
        arguments: true,
        confirmation: false,
        destructive: false,
    },
    Action {
        kind: ActionKind::IssueKey,
        title: "issue key",
        hint: "issue <name> <role> <database>...",
        arguments: true,
        confirmation: false,
        destructive: false,
    },
    Action {
        kind: ActionKind::RevokeKey,
        title: "revoke key",
        hint: "revoke <name>",
        arguments: true,
        confirmation: true,
        destructive: true,
    },
    Action {
        kind: ActionKind::RotateOwner,
        title: "rotate owner key",
        hint: "the old owner key dies the moment the new one is issued",
        arguments: false,
        confirmation: true,
        destructive: true,
    },
    Action {
        kind: ActionKind::Quit,
        title: "quit",
        hint: "leave the view",
        arguments: false,
        confirmation: false,
        destructive: false,
    },
];

/// The action of a kind, from the catalogue.
pub fn action(kind: ActionKind) -> &'static Action {
    CATALOG
        .iter()
        .find(|action| action.kind == kind)
        .expect("every kind is in the catalogue")
}

/// What the operator asked for, once the arguments are read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invocation {
    /// Read the service again.
    Refresh,
    /// Leave the view.
    Quit,
    /// Run one statement.
    Query { name: String, sql: String },
    /// List one database's tables.
    Tables { name: String },
    /// Read one database's table structure.
    Schema { name: String, table: Option<String> },
    /// Open a database as its own page: its tables and their rows.
    OpenDatabase { name: String },
    /// Read the browser's page again, after a page, sort or filter
    /// changed. It is not a question the operator asks directly: it is
    /// the browser re-asking its own, and it carries no arguments
    /// because the browser already holds them.
    RefreshRows,
    /// Page through one table's rows.
    OpenTable {
        name: String,
        table: String,
        page: u64,
    },
    /// Run a SQL file as one atomic batch.
    ExecuteFile { name: String, file: PathBuf },
    /// Run a JSON batch file.
    BatchFile { name: String, file: PathBuf },
    /// Create an empty database.
    CreateDatabase { name: String },
    /// Tombstone a database.
    DeleteDatabase { name: String },
    /// Copy a database.
    CloneDatabase { source: String, destination: String },
    /// Download a database to a file.
    ExportDatabase { name: String, out: PathBuf },
    /// Upload a file as a database.
    ImportDatabase { name: String, from: PathBuf },
    /// Issue a scoped credential.
    IssueKey {
        name: String,
        role: String,
        databases: Vec<String>,
    },
    /// Revoke a credential.
    RevokeKey { name: String },
    /// Replace the owner credential.
    RotateOwner,
}

/// How many rows one page of an opened table holds.
///
/// Ten is what the last-action pane draws (see `OUTCOME_ROWS` in the
/// draw layer), so a page is exactly what fits on screen: every row
/// the gateway returns for the page is drawn, and the next page holds
/// the next rows. A bigger page would fetch rows the pane could not
/// show, leaving most of each page unreachable.
pub const TABLE_PAGE_SIZE: u64 = 10;

/// Whether a table name is safe to put into a statement.
///
/// The name travels inside double quotes, and a quote inside it
/// would break out: rather than escaping, the view only opens names
/// made of letters, digits and underscores that do not start with a
/// digit. Everything the gateway creates for an operator matches.
pub fn valid_table(name: &str) -> bool {
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// The request an invocation makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The HTTP method.
    pub method: Method,
    /// The path, with the names already in it.
    pub path: String,
    /// The body, when the call has one.
    pub body: Option<Value>,
    /// What the request can commit.
    ///
    /// The method cannot say: a query is a POST whether it reads or
    /// writes. So the statements decide, with the same rule the server
    /// uses and the same rule the CLI uses. A read-only query that
    /// cannot reach the gateway is an unavailable gateway, never a write
    /// that may have committed.
    pub semantics: Semantics,
}

/// A refusal the operator can act on.
fn invalid(message: &'static str) -> Error {
    Error::new(400, "invalid_arguments", message)
}

/// The words of a line, with the quotes kept out of the way.
fn words(line: &str) -> Vec<&str> {
    line.split_whitespace().collect()
}

impl ActionKind {
    /// Read the operator's line into an invocation.
    ///
    /// The reading is per action, because the actions do not take the
    /// same arguments: a query's SQL is the rest of the line, and a
    /// key's scope is every word after the role.
    pub fn parse(self, arguments: &str) -> Result<Invocation> {
        let parts = words(arguments);
        let need = |count: usize, message: &'static str| -> Result<()> {
            if parts.len() < count {
                return Err(invalid(message));
            }
            Ok(())
        };
        match self {
            ActionKind::Refresh => Ok(Invocation::Refresh),
            ActionKind::Quit => Ok(Invocation::Quit),
            ActionKind::RotateOwner => Ok(Invocation::RotateOwner),
            ActionKind::Query => {
                need(2, "a query needs a database and a statement")?;
                let name = parts[0].to_string();
                let sql = arguments
                    .split_once(char::is_whitespace)
                    .map(|(_, rest)| rest.trim().to_string())
                    .unwrap_or_default();
                if sql.is_empty() {
                    return Err(invalid("a query needs a statement"));
                }
                crate::store::validate_name(&name)?;
                Ok(Invocation::Query { name, sql })
            }
            ActionKind::Tables => {
                need(1, "a table list needs a database")?;
                let name = parts[0].to_string();
                crate::store::validate_name(&name)?;
                Ok(Invocation::Tables { name })
            }
            ActionKind::Schema => {
                need(1, "a schema needs a database")?;
                let name = parts[0].to_string();
                crate::store::validate_name(&name)?;
                let table = match parts.get(1) {
                    Some(wanted) => {
                        if !valid_table(wanted) {
                            return Err(invalid(
                                "a table name is letters, digits and underscores, and does not start with a digit",
                            ));
                        }
                        Some(wanted.to_string())
                    }
                    None => None,
                };
                Ok(Invocation::Schema { name, table })
            }
            ActionKind::OpenTable => {
                need(2, "an open table needs a database and a table")?;
                let name = parts[0].to_string();
                let table = parts[1].to_string();
                crate::store::validate_name(&name)?;
                if !valid_table(&table) {
                    return Err(invalid(
                        "a table name is letters, digits and underscores, and does not start with a digit",
                    ));
                }
                let page = if parts.len() > 2 {
                    parts[2]
                        .parse::<u64>()
                        .map_err(|_| invalid("a page is a number, starting at 0"))?
                } else {
                    0
                };
                Ok(Invocation::OpenTable { name, table, page })
            }
            ActionKind::CreateDatabase => {
                need(1, "a new database needs a name")?;
                let name = parts[0].to_string();
                crate::store::validate_name(&name)?;
                Ok(Invocation::CreateDatabase { name })
            }
            ActionKind::DeleteDatabase => {
                need(1, "a deletion needs the name of the database")?;
                let name = parts[0].to_string();
                crate::store::validate_name(&name)?;
                Ok(Invocation::DeleteDatabase { name })
            }
            ActionKind::CloneDatabase => {
                need(2, "a clone needs a source and a destination")?;
                let source = parts[0].to_string();
                let destination = parts[1].to_string();
                crate::store::validate_name(&source)?;
                crate::store::validate_name(&destination)?;
                Ok(Invocation::CloneDatabase {
                    source,
                    destination,
                })
            }
            ActionKind::ExportDatabase => {
                need(2, "an export needs a database and a file")?;
                let name = parts[0].to_string();
                crate::store::validate_name(&name)?;
                Ok(Invocation::ExportDatabase {
                    name,
                    out: PathBuf::from(parts[1]),
                })
            }
            ActionKind::ImportDatabase => {
                need(2, "an import needs a database and a file")?;
                let name = parts[0].to_string();
                crate::store::validate_name(&name)?;
                Ok(Invocation::ImportDatabase {
                    name,
                    from: PathBuf::from(parts[1]),
                })
            }
            ActionKind::IssueKey => {
                need(3, "a key needs a name, a role and at least one database")?;
                let name = parts[0].to_string();
                let role = parts[1].to_string();
                if role != "database_read_only" && role != "database_read_write" {
                    return Err(invalid(
                        "a key's role is database_read_only or database_read_write",
                    ));
                }
                let databases: Vec<String> =
                    parts[2..].iter().map(|name| name.to_string()).collect();
                for database in &databases {
                    crate::store::validate_name(database)?;
                }
                Ok(Invocation::IssueKey {
                    name,
                    role,
                    databases,
                })
            }
            ActionKind::RevokeKey => {
                need(1, "a revocation needs the name of the credential")?;
                let name = parts[0].to_string();
                crate::store::validate_name(&name)?;
                Ok(Invocation::RevokeKey { name })
            }
            ActionKind::ExecuteFile => {
                need(2, "a script needs a database and a file")?;
                let name = parts[0].to_string();
                crate::store::validate_name(&name)?;
                Ok(Invocation::ExecuteFile {
                    name,
                    file: PathBuf::from(parts[1]),
                })
            }
            ActionKind::BatchFile => {
                need(2, "a batch needs a database and a file")?;
                let name = parts[0].to_string();
                crate::store::validate_name(&name)?;
                Ok(Invocation::BatchFile {
                    name,
                    file: PathBuf::from(parts[1]),
                })
            }
        }
    }
}

impl Invocation {
    /// The phrase the operator has to type to confirm a destructive
    /// act, when the act needs one.
    ///
    /// For a deletion and a revocation the phrase is the name of the
    /// thing: the operator has to spell it out, so nothing is
    /// destroyed by a stray keypress. For the owner key there is no
    /// name, and the word that says "yes, replace it" is the phrase.
    pub fn confirmation(&self) -> Option<String> {
        match self {
            Invocation::DeleteDatabase { name } => Some(name.clone()),
            Invocation::RevokeKey { name } => Some(name.clone()),
            Invocation::RotateOwner => Some("rotate".to_string()),
            _ => None,
        }
    }

    /// The request this invocation makes.
    ///
    /// The file actions read their file here, so an unreadable file or
    /// an over-long script is refused before anything is sent — by the
    /// same rules the CLI refuses it by.
    pub fn request(&self) -> Result<Request> {
        let request = match self {
            Invocation::Refresh
            | Invocation::RefreshRows
            | Invocation::OpenDatabase { .. }
            | Invocation::Quit => {
                return Err(invalid("this action makes no request"));
            }
            Invocation::Query { name, sql } => Request {
                method: Method::POST,
                path: format!("/v1/databases/{name}/query"),
                body: Some(json!({"sql": sql, "args": []})),
                semantics: crate::client::semantics_of(std::slice::from_ref(sql)),
            },
            Invocation::Tables { name } => Request {
                method: Method::GET,
                path: format!("/v1/databases/{name}"),
                body: None,
                semantics: Semantics::Read,
            },
            Invocation::Schema { name, table } => {
                // The table narrows the answer on the server, so the
                // document the view reads is the API's own either way.
                let path = match table {
                    Some(wanted) => format!("/v1/databases/{name}/schema?table={wanted}"),
                    None => format!("/v1/databases/{name}/schema"),
                };
                Request {
                    method: Method::GET,
                    path,
                    body: None,
                    semantics: Semantics::Read,
                }
            }
            Invocation::OpenTable { name, table, page } => {
                let offset = page.saturating_mul(TABLE_PAGE_SIZE);
                let sql = format!(
                    "SELECT * FROM \"{table}\" ORDER BY rowid LIMIT {TABLE_PAGE_SIZE} OFFSET {offset}"
                );
                Request {
                    method: Method::POST,
                    path: format!("/v1/databases/{name}/query"),
                    body: Some(json!({"sql": sql, "args": []})),
                    semantics: crate::client::semantics_of(std::slice::from_ref(&sql)),
                }
            }
            Invocation::CreateDatabase { name } => Request {
                method: Method::POST,
                path: "/v1/databases".to_string(),
                body: Some(json!({"name": name})),
                semantics: Semantics::Write,
            },
            Invocation::DeleteDatabase { name } => Request {
                method: Method::DELETE,
                path: format!("/v1/databases/{name}"),
                body: None,
                semantics: Semantics::Write,
            },
            Invocation::CloneDatabase {
                source,
                destination,
            } => Request {
                method: Method::POST,
                path: format!("/v1/databases/{source}/clone"),
                body: Some(json!({"destination": destination})),
                semantics: Semantics::Write,
            },
            Invocation::IssueKey {
                name,
                role,
                databases,
            } => Request {
                method: Method::POST,
                path: "/v1/keys".to_string(),
                body: Some(json!({"name": name, "role": role, "databases": databases})),
                semantics: Semantics::Write,
            },
            Invocation::RevokeKey { name } => Request {
                method: Method::DELETE,
                path: format!("/v1/keys/{name}"),
                body: None,
                semantics: Semantics::Write,
            },
            Invocation::RotateOwner => Request {
                method: Method::POST,
                path: "/v1/keys/owner/rotate".to_string(),
                body: None,
                semantics: Semantics::Write,
            },
            Invocation::ExecuteFile { name, file } => {
                let script = std::fs::read_to_string(file)
                    .map_err(|_| Error::new(400, "unreadable_file", "cannot read the SQL file"))?;
                let statements = crate::sqlsplit::split_statements(&script);
                if statements.is_empty() {
                    return Err(Error::new(
                        400,
                        "empty_script",
                        "the SQL file holds no statements",
                    ));
                }
                // The same cap the CLI enforces, checked before the
                // request is built: nothing is sent, and the refusal
                // says how many statements the file holds.
                crate::execution::check_statement_count(statements.len(), "the file")?;
                Request {
                    method: Method::POST,
                    path: format!("/v1/databases/{name}/batch"),
                    body: Some(json!({
                        "statements": statements.iter().map(|sql| json!({"sql": sql, "args": []})).collect::<Vec<_>>(),
                        "atomic": true,
                    })),
                    semantics: crate::client::semantics_of(&statements),
                }
            }
            Invocation::BatchFile { name, file } => {
                // The same file the CLI reads: a regular file, at most
                // 64 KiB, holding the API's own batch shape.
                let batch = crate::execution::read_batch_file(file)?;
                crate::execution::check_statement_count(batch.statements.len(), "the batch file")?;
                Request {
                    method: Method::POST,
                    path: format!("/v1/databases/{name}/batch"),
                    body: Some(serde_json::to_value(&batch).map_err(|_| Error::invalid())?),
                    semantics: crate::client::semantics_of_statements(&batch.statements),
                }
            }
            // The two transfers are not single requests: they are
            // chunked, and the library owns the whole sequence.
            Invocation::ExportDatabase { .. } | Invocation::ImportDatabase { .. } => {
                return Err(invalid("this action is a transfer, not a request"));
            }
        };
        Ok(request)
    }

    /// The statements a request carries, when it carries any.
    ///
    /// The view reads the outcome of a batch through the shared
    /// interpretation, and that reading needs the statements the
    /// request sent.
    pub fn statements(&self) -> Vec<String> {
        match self {
            Invocation::ExecuteFile { .. } | Invocation::BatchFile { .. } => self
                .request()
                .ok()
                .and_then(|request| request.body)
                .and_then(|body| body["statements"].as_array().cloned())
                .map(|list| {
                    list.iter()
                        .filter_map(|statement| statement["sql"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_maps_to_the_documented_request() {
        // The view must not invent a call the CLI would not make: each
        // action's method and path are the API's own.
        let cases: Vec<(ActionKind, &str, Method, &str)> = vec![
            (
                ActionKind::Query,
                "demo select 1",
                Method::POST,
                "/v1/databases/demo/query",
            ),
            (
                ActionKind::CreateDatabase,
                "demo",
                Method::POST,
                "/v1/databases",
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
            let request = invocation
                .request()
                .expect("the invocation makes a request");
            assert_eq!(request.method, method, "{kind:?} uses the wrong method");
            assert_eq!(request.path, path, "{kind:?} uses the wrong path");
        }
    }

    #[test]
    fn a_query_keeps_the_whole_statement() {
        // The SQL is the rest of the line: a statement with spaces in
        // it is a statement, not two words.
        let invocation = ActionKind::Query
            .parse("demo select * from users where name = 'a b'")
            .expect("the arguments parse");
        assert_eq!(
            invocation,
            Invocation::Query {
                name: "demo".to_string(),
                sql: "select * from users where name = 'a b'".to_string(),
            }
        );
    }

    #[test]
    fn a_destructive_action_asks_for_the_exact_name() {
        let invocation = ActionKind::DeleteDatabase
            .parse("demo")
            .expect("the arguments parse");
        assert_eq!(invocation.confirmation().as_deref(), Some("demo"));
        let revoke = ActionKind::RevokeKey
            .parse("robot")
            .expect("the arguments parse");
        assert_eq!(revoke.confirmation().as_deref(), Some("robot"));
        let rotate = ActionKind::RotateOwner.parse("").expect("no arguments");
        assert_eq!(rotate.confirmation().as_deref(), Some("rotate"));
    }

    #[test]
    fn a_key_takes_a_role_and_at_least_one_database() {
        let invocation = ActionKind::IssueKey
            .parse("robot database_read_only demo other")
            .expect("the arguments parse");
        assert_eq!(
            invocation,
            Invocation::IssueKey {
                name: "robot".to_string(),
                role: "database_read_only".to_string(),
                databases: vec!["demo".to_string(), "other".to_string()],
            }
        );
        assert!(
            ActionKind::IssueKey
                .parse("robot database_read_only")
                .is_err(),
            "a key with no database was accepted"
        );
        assert!(
            ActionKind::IssueKey.parse("robot owner demo").is_err(),
            "a key with an unknown role was accepted"
        );
    }

    #[test]
    fn an_over_long_script_is_refused_before_it_is_sent() {
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
    fn a_script_at_the_cap_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("script.sql");
        let script = (0..32)
            .map(|index| format!("select {index};"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file, script).unwrap();
        let invocation = ActionKind::ExecuteFile
            .parse(&format!("demo {}", file.display()))
            .expect("the arguments parse");
        let request = invocation.request().expect("a script of 32 statements");
        assert_eq!(
            request.body.unwrap()["statements"]
                .as_array()
                .unwrap()
                .len(),
            32
        );
    }
}
