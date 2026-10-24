//! The statement shapes the service has run, and what each one cost.
//!
//! "Which query is heavy?" is a question only the service itself can
//! answer, and a hosted provider is structurally unable to answer it
//! exactly: it is a privacy liability and a tenant-isolation problem
//! for them, and nothing in their business depends on the answer. Here
//! there is no middleman. The gateway sees every statement on its way
//! to SQLite, and the operator owns the box it all happens on.
//!
//! What is kept is the SHAPE of a statement -- its SQL with every
//! literal replaced by a placeholder -- never the values it carried.
//! `SELECT * FROM users WHERE email = 'ada@example.com'` is kept as
//! `SELECT * FROM users WHERE email = ?`, so the record answers "how
//! much does this statement cost" without becoming a copy of the data.
//! Column and table names survive, because they are the schema, and the
//! schema is the thing an operator needs to see.
//!
//! The store is bounded in two directions: a database keeps a fixed
//! number of distinct shapes, and a shape attributes itself to a fixed
//! number of credentials. Both drop the least useful entry rather than
//! growing without limit, because a query log that fills the disk is
//! the same failure as any other disk exhaustion.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// How many distinct statement shapes one database keeps.
///
/// The cap is per database: the question "which query is heavy in THIS
/// database" is the one that gets asked, and a global cap would let a
/// chatty database hide the answers for every other one.
pub const MAX_SHAPES_PER_DATABASE: usize = 200;

/// How many credentials one shape attributes itself to.
///
/// A shape run by many credentials is a service-wide pattern; the
/// per-credential detail that matters is on the shapes that only a few
/// credentials touch.
pub const MAX_PRINCIPALS_PER_SHAPE: usize = 16;

/// One statement shape, and what it has cost.
#[derive(Debug, Clone, Default)]
pub struct QueryShape {
    /// The SQL with every literal replaced by `?`.
    pub shape: String,
    /// How many times it ran.
    pub count: u64,
    /// Rows it returned.
    pub rows_read: u64,
    /// Rows it changed.
    pub rows_written: u64,
    /// The total time it took.
    pub total_micros: u64,
    /// Its slowest single run.
    pub max_micros: u64,
    /// When it first ran.
    pub first_seen: u64,
    /// When it last ran.
    pub last_seen: u64,
    /// Which credentials ran it, and how often each did.
    pub principals: HashMap<String, u64>,
}

/// One statement's shape and what it cost, as the recorder is handed it.
#[derive(Debug, Clone)]
pub struct StatementRun {
    /// The statement's SQL.
    pub sql: String,
    /// Rows it returned.
    pub rows_read: u64,
    /// Rows it changed.
    pub rows_written: u64,
    /// How long it took.
    pub micros: u64,
}

/// The statement shapes the service has run, per database.
#[derive(Debug, Default)]
pub struct QueryLog {
    shapes: Mutex<HashMap<String, HashMap<String, QueryShape>>>,
}

impl QueryLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record what a set of statements cost.
    pub fn record(&self, database: &str, principal: &str, runs: &[StatementRun]) {
        if runs.is_empty() {
            return;
        }
        let now = now_secs();
        let mut guard = match self.shapes.lock() {
            Ok(guard) => guard,
            // A poisoned lock means a previous holder panicked. The data
            // behind it is still a query log: losing the ability to
            // record is worse than the log itself.
            Err(poisoned) => poisoned.into_inner(),
        };
        let per_database = guard.entry(database.to_string()).or_default();
        for run in runs {
            let shape = normalise(&run.sql);
            if shape.is_empty() {
                continue;
            }
            let entry = per_database
                .entry(shape.clone())
                .or_insert_with(|| QueryShape {
                    shape: shape.clone(),
                    first_seen: now,
                    ..Default::default()
                });
            entry.count += 1;
            entry.rows_read += run.rows_read;
            entry.rows_written += run.rows_written;
            entry.total_micros += run.micros;
            entry.max_micros = entry.max_micros.max(run.micros);
            entry.last_seen = now;
            if entry.principals.len() < MAX_PRINCIPALS_PER_SHAPE
                || entry.principals.contains_key(principal)
            {
                *entry.principals.entry(principal.to_string()).or_insert(0) += 1;
            }
        }
        // Bounded: the least recently seen shape goes first, because the
        // recent past is the part an operator can act on.
        while per_database.len() > MAX_SHAPES_PER_DATABASE {
            let oldest = per_database
                .iter()
                .min_by_key(|(_, shape)| shape.last_seen)
                .map(|(name, _)| name.clone());
            match oldest {
                Some(name) => {
                    per_database.remove(&name);
                }
                None => break,
            }
        }
    }

    /// The shapes recorded for one database, most expensive first.
    pub fn for_database(&self, database: &str) -> Vec<QueryShape> {
        let guard = match self.shapes.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut shapes: Vec<QueryShape> = guard
            .get(database)
            .map(|shapes| shapes.values().cloned().collect())
            .unwrap_or_default();
        shapes.sort_by(|a, b| {
            b.rows_read
                .cmp(&a.rows_read)
                .then(b.total_micros.cmp(&a.total_micros))
                .then(a.shape.cmp(&b.shape))
        });
        shapes
    }

    /// Every database that has recorded something, with its shapes.
    pub fn all(&self) -> Vec<(String, Vec<QueryShape>)> {
        let guard = match self.shapes.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut out: Vec<(String, Vec<QueryShape>)> = guard
            .iter()
            .map(|(database, shapes)| {
                let mut shapes: Vec<QueryShape> = shapes.values().cloned().collect();
                shapes.sort_by(|a, b| {
                    b.rows_read
                        .cmp(&a.rows_read)
                        .then(b.total_micros.cmp(&a.total_micros))
                        .then(a.shape.cmp(&b.shape))
                });
                (database.clone(), shapes)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Drop a purged database's shapes.
    pub fn forget(&self, database: &str) {
        let mut guard = match self.shapes.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.remove(database);
    }
}

/// Seconds since the Unix epoch, or zero if the clock is before it.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// A statement's shape: its SQL with every literal replaced by a
/// placeholder, so two runs of the same statement are one shape.
///
/// The rules, in the order they are applied to the text:
///
/// - a string literal becomes `?` (and the literal's contents never
///   reach the log),
/// - a number becomes `?`,
/// - a bound parameter (`:name`, `@name`, `$name`) becomes `?`,
/// - a quoted identifier (`"name"`, `` `name` ``, `[name]`) is kept,
///   because a column name is schema rather than data,
/// - a comment is dropped,
/// - runs of whitespace become one space.
///
/// What is deliberately NOT normalised: the case of keywords, and the
/// order of clauses. Merging `select` with `SELECT` would need a
/// keyword list, and a keyword list is a parser's job; this is a
/// scanner, and a scanner that over-reaches is worse than one that
/// under-reaches.
pub fn normalise(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut space = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                // A string literal: consume it whole, honouring the
                // doubled-quote escape, and keep none of it.
                out.push('?');
                space = false;
                while let Some(inner) = chars.next() {
                    if inner == '\'' {
                        // Two quotes in a row are one escaped quote and
                        // the literal goes on; a lone quote ends it.
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                            continue;
                        }
                        break;
                    }
                }
            }
            '"' | '`' => {
                // A quoted identifier: keep it as it is.
                if space && !out.is_empty() {
                    out.push(' ');
                }
                space = false;
                out.push(c);
                for inner in chars.by_ref() {
                    out.push(inner);
                    if inner == c {
                        break;
                    }
                }
            }
            '[' => {
                if space && !out.is_empty() {
                    out.push(' ');
                }
                space = false;
                out.push(c);
                while let Some(inner) = chars.next() {
                    out.push(inner);
                    if inner == ']' {
                        break;
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                // A line comment: drop it to the end of the line.
                for inner in chars.by_ref() {
                    if inner == '\n' {
                        break;
                    }
                }
                space = true;
            }
            '/' if chars.peek() == Some(&'*') => {
                // A block comment: drop it to its end.
                chars.next();
                while let Some(inner) = chars.next() {
                    if inner == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        break;
                    }
                }
                space = true;
            }
            ':' | '@' | '$' => {
                // A named bound parameter: `:name`, `@name`, `$name`.
                let mut name = String::new();
                while let Some(&inner) = chars.peek() {
                    if inner.is_alphanumeric() || inner == '_' {
                        name.push(inner);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if name.is_empty() {
                    if space && !out.is_empty() {
                        out.push(' ');
                    }
                    space = false;
                    out.push(c);
                } else {
                    if space && !out.is_empty() {
                        out.push(' ');
                    }
                    space = false;
                    out.push('?');
                }
            }
            c if c.is_ascii_digit() => {
                // A number: consume the whole literal.
                if space && !out.is_empty() {
                    out.push(' ');
                }
                space = false;
                out.push('?');
                while let Some(&inner) = chars.peek() {
                    if inner.is_ascii_alphanumeric() || inner == '.' || inner == '_' {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                // An identifier or a keyword: consume the whole word, so
                // that `table2` keeps its digit.
                if space && !out.is_empty() {
                    out.push(' ');
                }
                space = false;
                out.push(c);
                while let Some(&inner) = chars.peek() {
                    if inner.is_ascii_alphanumeric() || inner == '_' || inner == '$' {
                        out.push(inner);
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            c if c.is_whitespace() => {
                space = true;
            }
            c => {
                if space && !out.is_empty() {
                    out.push(' ');
                }
                space = false;
                out.push(c);
            }
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_never_survive_into_the_shape() {
        // The point of the shape: the statement without its data.
        assert_eq!(
            normalise("SELECT * FROM users WHERE email = 'ada@example.com'"),
            "SELECT * FROM users WHERE email = ?"
        );
        assert_eq!(
            normalise("SELECT * FROM users WHERE id = 42"),
            "SELECT * FROM users WHERE id = ?"
        );
        assert_eq!(
            normalise("SELECT * FROM users WHERE name = :who"),
            "SELECT * FROM users WHERE name = ?"
        );
        assert_eq!(
            normalise("INSERT INTO t VALUES (?, 'x')"),
            "INSERT INTO t VALUES (?, ?)"
        );
    }

    #[test]
    fn identifiers_and_schema_survive() {
        // A column name is schema, and the schema is what an operator
        // needs to see.
        assert_eq!(
            normalise("SELECT \"email\" FROM users"),
            "SELECT \"email\" FROM users"
        );
        assert_eq!(
            normalise("SELECT `email` FROM users"),
            "SELECT `email` FROM users"
        );
        assert_eq!(
            normalise("SELECT [email] FROM users"),
            "SELECT [email] FROM users"
        );
        // A digit inside an identifier is part of the name.
        assert_eq!(normalise("SELECT a1 FROM t2"), "SELECT a1 FROM t2");
    }

    #[test]
    fn comments_and_spacing_do_not_make_a_new_shape() {
        assert_eq!(
            normalise("SELECT  a\nFROM   t -- a comment\n"),
            "SELECT a FROM t"
        );
        assert_eq!(
            normalise("SELECT /* a comment */ a FROM t"),
            "SELECT a FROM t"
        );
    }

    #[test]
    fn a_quote_inside_a_literal_does_not_end_it() {
        assert_eq!(
            normalise("SELECT * FROM t WHERE a = 'it''s here' AND b = 1"),
            "SELECT * FROM t WHERE a = ? AND b = ?"
        );
    }

    #[test]
    fn the_log_keeps_the_most_useful_shapes_and_drops_the_rest() {
        let log = QueryLog::new();
        // More distinct shapes than the cap.
        for index in 0..(MAX_SHAPES_PER_DATABASE + 10) {
            log.record(
                "alpha",
                "owner",
                &[StatementRun {
                    sql: format!("SELECT {index} FROM t"),
                    rows_read: 1,
                    rows_written: 0,
                    micros: 10,
                }],
            );
        }
        let shapes = log.for_database("alpha");
        assert_eq!(shapes.len(), MAX_SHAPES_PER_DATABASE);
    }

    #[test]
    fn the_log_aggregates_by_shape_and_keeps_the_heaviest_first() {
        let log = QueryLog::new();
        log.record(
            "alpha",
            "owner",
            &[StatementRun {
                sql: "SELECT * FROM t WHERE id = 1".into(),
                rows_read: 10,
                rows_written: 0,
                micros: 100,
            }],
        );
        log.record(
            "alpha",
            "app",
            &[StatementRun {
                sql: "SELECT * FROM t WHERE id = 2".into(),
                rows_read: 5,
                rows_written: 0,
                micros: 50,
            }],
        );
        let shapes = log.for_database("alpha");
        assert_eq!(shapes.len(), 1, "the two literals are one shape");
        assert_eq!(shapes[0].count, 2);
        assert_eq!(shapes[0].rows_read, 15);
        assert_eq!(shapes[0].total_micros, 150);
        assert_eq!(shapes[0].max_micros, 100);
        assert_eq!(shapes[0].principals.len(), 2);
    }
}
