//! Property fuzzing of the hand-written SQL scanners and of the authorizer.
//!
//! Run: `cargo test --locked --lib execution::fuzz`
//! More cases: `DG_FUZZ_CASES=200000 cargo test --release --locked --lib execution::fuzz`
//!
//! The gateway reads SQL text with small scanners of its own -- the
//! script splitter, the read/write lane classifier, the REINDEX shape
//! check, the query-log normaliser -- and judges every statement with an
//! authorizer. Each is checked here against SQLite itself on generated
//! adversarial input: semicolons hidden in literals, comments and trigger
//! bodies; protected names quoted, bracketed, schema-qualified and
//! case-folded; keywords inside strings and comments; `--` inside block
//! comments; EXPLAIN, CTEs, subqueries and table-valued pragmas. A
//! seeded generator makes every failure reproducible from the seed and
//! case number it prints.
//!
//! Properties:
//!
//! 1. Authorizer, write credential: a statement that prepares under the
//!    gateway's authorizer touched no `_dg_*` object.
//! 2. Authorizer, read-only credential: such a statement also performed
//!    no action beyond reading, selecting, calling a function and the
//!    introspection PRAGMAs.
//! 3. Lane classifier: a statement put on the read lane is one SQLite
//!    calls read-only.
//! 4. REINDEX shape: a `CREATE [UNIQUE] INDEX` SQLite accepts is one the
//!    gateway lets build its index.
//! 5. Splitter: a script made of known statements splits back into
//!    exactly those statements.
//! 6. Query log: a normalised statement keeps no string-literal content,
//!    no comment content and no numeric literal.
//! 7. None of the scanners panics on arbitrary text.

use super::*;
use rusqlite::hooks::AuthAction;
use std::sync::{Arc, Mutex};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
    fn pick<'a>(&mut self, items: &'a [&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

fn cases() -> u64 {
    std::env::var("DG_FUZZ_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4000)
}

fn seed() -> u64 {
    std::env::var("DG_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x5EED)
}

/// A database shaped like a real one: the gateway's own table beside an
/// application's tables, an index, a view and a trigger.
fn fixture() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE _dg_replay (principal TEXT NOT NULL, key TEXT NOT NULL, hash TEXT NOT NULL, \
           result TEXT NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(principal,key));
         CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, key TEXT);
         CREATE TABLE \"odd name\"(x, y);
         CREATE TABLE log(n INTEGER, key TEXT);
         CREATE INDEX users_name ON users(name);
         CREATE VIEW uv AS SELECT id, name FROM users;
         CREATE TRIGGER users_log AFTER INSERT ON users BEGIN INSERT INTO log VALUES (new.id, 'x'); END;",
    )
    .unwrap();
    conn
}

// ------------------------------------------------------------ generator

const TABLES: &[&str] = &[
    "users",
    "\"users\"",
    "[users]",
    "`users`",
    "main.users",
    "USERS",
    "\"odd name\"",
    "log",
    "uv",
    "sqlite_master",
    "sqlite_schema",
    "_dg_replay",
    "_DG_REPLAY",
    "_Dg_RePlAy",
    "\"_dg_replay\"",
    "[_dg_replay]",
    "`_dg_replay`",
    "main._dg_replay",
    "main.\"_dg_replay\"",
    "\"main\".\"_dg_replay\"",
    "_dg_other",
    "pragma_table_info('_dg_replay')",
    "pragma_index_list('_dg_replay')",
    "pragma_table_info('users')",
    "json_each('[1,2]')",
];

const NAMES: &[&str] = &[
    "fresh",
    "\"fresh name\"",
    "_dg_new",
    "\"_DG_NEW\"",
    "[_dg_new]",
    "main._dg_new",
    "users_name",
    "sqlite_autoindex__dg_replay_1",
];

const LITERALS: &[&str] = &[
    "1",
    "'a'",
    "'it''s'",
    "'DELETE FROM users'",
    "'; DROP TABLE users; --'",
    "'_dg_replay'",
    "'/* not a comment */'",
    "'-- nor this'",
    "X'00ff'",
    "NULL",
    "-1.5e3",
    "'ünï 🦀'",
    "?",
    ":name",
];

/// Whitespace in all its spellings, including comments that hold the very
/// tokens a careless scanner would act on.
fn gap(rng: &mut Rng) -> String {
    match rng.below(14) {
        0 => "\n".into(),
        1 => "\t".into(),
        2 => "  ".into(),
        3 => "/*x*/".into(),
        4 => " /* DELETE ; _dg_replay */ ".into(),
        5 => " -- DELETE ; _dg_replay\n".into(),
        6 => " /*--*/ ".into(),
        7 => " /* -- ; */ ".into(),
        8 => "\r\n".into(),
        _ => " ".into(),
    }
}

fn kw(rng: &mut Rng, word: &str) -> String {
    match rng.below(4) {
        0 => word.to_ascii_lowercase(),
        1 => word
            .chars()
            .enumerate()
            .map(|(i, c)| {
                if i % 2 == 0 {
                    c.to_ascii_lowercase()
                } else {
                    c
                }
            })
            .collect(),
        _ => word.to_string(),
    }
}

/// Join tokens with random gaps.
fn sql(rng: &mut Rng, tokens: &[String]) -> String {
    let mut out = String::new();
    for (i, token) in tokens.iter().enumerate() {
        if i > 0 {
            out.push_str(&gap(rng));
        }
        out.push_str(token);
    }
    out
}

fn statement(rng: &mut Rng, depth: usize) -> String {
    let t = rng.pick(TABLES).to_string();
    let t2 = rng.pick(TABLES).to_string();
    let name = rng.pick(NAMES).to_string();
    let lit = rng.pick(LITERALS).to_string();
    let col = rng
        .pick(&["rowid", "*", "name", "key", "x", "1"])
        .to_string();
    let k = |rng: &mut Rng, w: &str| kw(rng, w);
    let tokens: Vec<String> = match rng.below(30) {
        0 => vec![k(rng, "SELECT"), "*".into(), k(rng, "FROM"), t],
        1 => vec![
            k(rng, "SELECT"),
            lit.clone(),
            ",".into(),
            "count(*)".into(),
            k(rng, "FROM"),
            t,
            k(rng, "WHERE"),
            col,
            "=".into(),
            lit,
        ],
        2 => vec![
            k(rng, "WITH"),
            "x".into(),
            k(rng, "AS"),
            format!("({})", statement_select(rng, &t)),
            k(rng, "SELECT"),
            "*".into(),
            k(rng, "FROM"),
            "x".into(),
        ],
        3 => vec![
            k(rng, "WITH"),
            "x(a)".into(),
            k(rng, "AS"),
            "(SELECT 1)".into(),
            k(rng, "INSERT"),
            k(rng, "INTO"),
            t,
            k(rng, "SELECT"),
            "*".into(),
            k(rng, "FROM"),
            "x".into(),
        ],
        4 => vec![
            k(rng, "INSERT"),
            k(rng, "INTO"),
            t,
            k(rng, "VALUES"),
            format!("({lit})"),
        ],
        5 => vec![
            k(rng, "INSERT"),
            k(rng, "INTO"),
            t,
            k(rng, "SELECT"),
            "*".into(),
            k(rng, "FROM"),
            t2,
        ],
        6 => vec![k(rng, "UPDATE"), t, k(rng, "SET"), col, "=".into(), lit],
        7 => vec![k(rng, "DELETE"), k(rng, "FROM"), t],
        8 => vec![
            k(rng, "REPLACE"),
            k(rng, "INTO"),
            t,
            k(rng, "VALUES"),
            format!("({lit})"),
        ],
        9 => vec![
            k(rng, "CREATE"),
            k(rng, "INDEX"),
            name,
            k(rng, "ON"),
            t,
            format!("({col})"),
        ],
        10 => vec![
            k(rng, "CREATE"),
            k(rng, "UNIQUE"),
            k(rng, "INDEX"),
            k(rng, "IF"),
            k(rng, "NOT"),
            k(rng, "EXISTS"),
            name,
            k(rng, "ON"),
            t,
            format!("({col})"),
        ],
        11 => vec![k(rng, "REINDEX")],
        12 => vec![k(rng, "REINDEX"), t],
        13 => vec![k(rng, "REINDEX"), name],
        14 if depth < 2 => vec![k(rng, "EXPLAIN"), statement(rng, depth + 1)],
        15 if depth < 2 => vec![
            k(rng, "EXPLAIN"),
            k(rng, "QUERY"),
            k(rng, "PLAN"),
            statement(rng, depth + 1),
        ],
        16 => vec![k(rng, "DROP"), k(rng, "TABLE"), t],
        17 => vec![k(rng, "DROP"), k(rng, "INDEX"), name],
        18 => vec![
            k(rng, "ALTER"),
            k(rng, "TABLE"),
            t,
            k(rng, "ADD"),
            k(rng, "COLUMN"),
            "z".into(),
        ],
        19 => vec![
            k(rng, "ALTER"),
            k(rng, "TABLE"),
            t,
            k(rng, "RENAME"),
            k(rng, "TO"),
            name,
        ],
        20 => vec![
            k(rng, "CREATE"),
            k(rng, "VIEW"),
            name,
            k(rng, "AS"),
            statement_select(rng, &t),
        ],
        21 => vec![
            k(rng, "CREATE"),
            k(rng, "TRIGGER"),
            name,
            k(rng, "AFTER"),
            k(rng, "INSERT"),
            k(rng, "ON"),
            t,
            k(rng, "BEGIN"),
            format!("{};", trigger_body(rng, &t2)),
            k(rng, "END"),
        ],
        22 => vec![
            k(rng, "PRAGMA"),
            format!(
                "{}({})",
                rng.pick(&[
                    "table_info",
                    "index_list",
                    "quick_check",
                    "main.table_info",
                    "integrity_check"
                ]),
                t
            ),
        ],
        23 => vec![
            k(rng, "PRAGMA"),
            rng.pick(&[
                "foreign_keys=OFF",
                "journal_mode=DELETE",
                "quick_check",
                "writable_schema=ON",
                "table_list",
            ])
            .into(),
        ],
        24 => vec![
            k(rng, "SELECT"),
            "*".into(),
            k(rng, "FROM"),
            format!(
                "pragma_{}('{}')",
                rng.pick(&[
                    "table_info",
                    "index_list",
                    "foreign_key_list",
                    "quick_check"
                ]),
                rng.pick(&["users", "_dg_replay", "_DG_REPLAY", "log"])
            ),
        ],
        25 => vec![
            k(rng, "SELECT"),
            format!("(SELECT count(*) FROM {t})"),
            ",".into(),
            format!("(SELECT max(rowid) FROM {t2})"),
        ],
        26 => vec![
            k(rng, "SELECT"),
            "*".into(),
            k(rng, "FROM"),
            t,
            "a".into(),
            k(rng, "JOIN"),
            t2,
            "b".into(),
            k(rng, "ON"),
            "a.rowid = b.rowid".into(),
        ],
        27 => vec![
            k(rng, "VALUES"),
            format!("({lit})"),
            ",".into(),
            "(2)".into(),
        ],
        28 => {
            let word = rng
                .pick(&[
                    "BEGIN",
                    "COMMIT",
                    "ROLLBACK",
                    "SAVEPOINT s",
                    "VACUUM",
                    "ANALYZE",
                ])
                .to_string();
            vec![k(rng, &word)]
        }
        _ => vec![
            k(rng, "ATTACH"),
            "':memory:'".into(),
            k(rng, "AS"),
            "other".into(),
        ],
    };
    let mut text = sql(rng, &tokens);
    if rng.chance(10) {
        text.push(';');
    }
    text
}

fn statement_select(rng: &mut Rng, table: &str) -> String {
    let tokens = vec![
        kw(rng, "SELECT"),
        "*".into(),
        kw(rng, "FROM"),
        table.to_string(),
    ];
    sql(rng, &tokens)
}

fn trigger_body(rng: &mut Rng, table: &str) -> String {
    match rng.below(4) {
        0 => format!("DELETE FROM {table}"),
        1 => format!("INSERT INTO {table} SELECT * FROM {table}"),
        2 => format!("UPDATE {table} SET rowid = rowid"),
        _ => "SELECT 1".into(),
    }
}

fn garbage(rng: &mut Rng) -> String {
    const ALPHABET: &[&str] = &[
        "'", "\"", "`", "[", "]", "-", "--", "/", "*", "/*", "*/", ";", "\n", " ", "a", "Z", "_",
        "_dg_", "0", "9", ".", "?", ":", "@", "$", "ü", "日", "🦀", "\u{0}", "\\", "x'", "E", "e",
        "SELECT", "CREATE", "INDEX", "REINDEX", "BEGIN", "END", "CASE",
    ];
    let n = rng.below(40);
    (0..n)
        .map(|_| ALPHABET[rng.below(ALPHABET.len())])
        .collect()
}

// ------------------------------------------------------------ properties

fn mentions_protected(action: &str) -> bool {
    action.to_ascii_lowercase().contains("\"_dg_")
}

/// Prepare `sql` under the gateway's authorizer, recording every action
/// SQLite asked about. `Some(actions)` when it prepared.
fn prepare_judged(
    conn: &mut Connection,
    sql: &str,
    read_only: bool,
) -> Option<(Vec<String>, bool)> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let may = may_reindex(sql);
    conn.authorizer(Some(move |ctx: AuthContext<'_>| {
        let verdict = authorize(ctx, read_only, may);
        if matches!(verdict, Authorization::Allow) {
            record.lock().unwrap().push(format!("{:?}", ctx.action));
        }
        verdict
    }))
    .unwrap();
    let outcome = conn.prepare(sql).map(|stmt| stmt.readonly()).ok();
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    outcome.map(|readonly| (seen.lock().unwrap().clone(), readonly))
}

fn fail(seed: u64, case: u64, what: &str, sql: &str, detail: &str) -> ! {
    panic!(
        "fuzz property failed: {what}\n  seed {seed} case {case} (DG_FUZZ_SEED={seed})\n  sql: {sql:?}\n  {detail}"
    )
}

#[test]
fn authorizer_never_lets_a_statement_touch_protected_objects() {
    let mut conn = fixture();
    let seed = seed();
    let mut rng = Rng(seed);
    let mut prepared = 0;
    for case in 0..cases() {
        let sql = statement(&mut rng, 0);
        for read_only in [false, true] {
            let Some((actions, readonly)) = prepare_judged(&mut conn, &sql, read_only) else {
                continue;
            };
            prepared += 1;
            if let Some(bad) = actions.iter().find(|a| mentions_protected(a)) {
                fail(
                    seed,
                    case,
                    "a prepared statement touched a _dg_ object",
                    &sql,
                    bad,
                );
            }
            if read_only {
                // What a read-only credential's statement may consist of.
                // Anything else that SQLite was asked about and allowed is a
                // write that the authorizer let through.
                let allowed = |a: &str| {
                    ["Read ", "Select", "Recursive", "Function ", "Pragma "]
                        .iter()
                        .any(|p| a.starts_with(p))
                };
                if let Some(bad) = actions.iter().find(|a| !allowed(a)) {
                    fail(
                        seed,
                        case,
                        "a read-only statement did more than read",
                        &sql,
                        bad,
                    );
                }
                // run_statement refuses a non-read-only statement for this
                // credential, so the gateway's real gate is both checks.
                let _ = readonly;
            }
        }
    }
    // A generator whose output never prepares checks nothing.
    assert!(
        prepared > cases() / 4,
        "only {prepared} statements prepared"
    );
}

#[test]
fn read_lane_statements_are_read_only_to_sqlite() {
    let conn = fixture();
    let seed = seed();
    let mut rng = Rng(seed ^ 1);
    for case in 0..cases() {
        let sql = statement(&mut rng, 0);
        if !is_read_only_one(sql.trim_start()) {
            continue;
        }
        if let Ok(stmt) = conn.prepare(&sql)
            && !stmt.readonly()
        {
            fail(seed, case, "a write was put on the read lane", &sql, "");
        }
    }
}

#[test]
fn every_index_sqlite_accepts_can_be_built() {
    let mut conn = fixture();
    let seed = seed();
    let mut rng = Rng(seed ^ 2);
    for case in 0..cases() {
        let sql = statement(&mut rng, 0);
        let seen = Arc::new(Mutex::new(false));
        let record = seen.clone();
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(ctx.action, AuthAction::CreateIndex { .. }) {
                *record.lock().unwrap() = true;
            }
            Authorization::Allow
        }))
        .unwrap();
        let ok = conn.prepare(&sql).is_ok();
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        // EXPLAIN of a CREATE INDEX also reports CreateIndex, and is refused
        // on purpose (see may_reindex), so it is not a legitimate build.
        let explained = strip_comments(&sql)
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("explain");
        if ok && *seen.lock().unwrap() && !explained && !may_reindex(&sql) {
            fail(
                seed,
                case,
                "a valid CREATE INDEX would be refused",
                &sql,
                "",
            );
        }
    }
}

#[test]
fn scripts_split_into_the_statements_they_were_built_from() {
    let conn = fixture();
    let seed = seed();
    let mut rng = Rng(seed ^ 3);
    let mut checked = 0;
    for case in 0..cases() {
        // Statements SQLite parses as exactly one statement each, with no
        // comment at either end (a leading comment is dropped by the
        // splitter and a trailing line comment would swallow the `;`).
        let mut parts = Vec::new();
        while parts.len() < 1 + rng.below(5) {
            let candidate = statement(&mut rng, 0);
            let candidate = candidate.trim().trim_end_matches(';').trim().to_string();
            let ends_cleanly = !candidate.ends_with("*/")
                && !candidate.starts_with("/*")
                && !candidate.starts_with("--")
                && !candidate.lines().last().unwrap_or("").contains("--");
            if ends_cleanly && !candidate.is_empty() && conn.prepare(&candidate).is_ok() {
                parts.push(candidate);
            }
        }
        let mut script = String::new();
        for (i, part) in parts.iter().enumerate() {
            if rng.chance(30) {
                script.push_str(rng.pick(&["-- lead; DELETE\n", "/* lead ; */", "\n\n", "  "]));
            }
            script.push_str(part);
            if i + 1 < parts.len() || rng.chance(70) {
                script.push(';');
            }
            script.push_str(rng.pick(&["\n", " ", "", "\n-- trailing ; note\n", " /* ; */\n"]));
        }
        let got = crate::sqlsplit::split_statements(&script);
        // A statement may keep a trailing comment (the last one in a script
        // with no closing `;` does); SQLite reads a comment as whitespace,
        // so that is the same statement. Anything else is not.
        let same = got.len() == parts.len()
            && got.iter().zip(&parts).all(|(g, p)| {
                g.strip_prefix(p.as_str())
                    .is_some_and(|rest| strip_comments(rest).trim().is_empty())
            });
        if !same {
            fail(
                seed,
                case,
                "a script did not split into its statements",
                &script,
                &format!("expected {parts:?}\n  got      {got:?}"),
            );
        }
        checked += 1;
    }
    assert_eq!(checked, cases());
}

#[test]
fn the_query_log_keeps_no_literal_or_comment_content() {
    let seed = seed();
    let mut rng = Rng(seed ^ 4);
    for case in 0..cases() {
        let marker = format!("SECRET{}", rng.below(1000));
        let number = 900_000_000 + rng.below(99_999_999);
        let literal = match rng.below(5) {
            0 => format!("'{marker}'"),
            1 => format!("'it''s {marker} '' ünï'"),
            2 => format!("'{marker} -- /* ; '"),
            3 => format!("X'{marker}'"),
            _ => format!("'🦀{marker}🦀'"),
        };
        let comment = match rng.below(3) {
            0 => format!("-- {marker}\n"),
            1 => format!("/* {marker} */"),
            _ => " ".into(),
        };
        let sql = format!(
            "SELECT {} {comment} FROM users WHERE name = {literal} AND id = {number}{}",
            rng.pick(&["*", "id", "\"name\""]),
            rng.pick(&["", " -- tail", " /* tail */", ";"])
        );
        let shape = crate::store::queries::normalise(&sql);
        if shape.contains(&marker) || shape.contains(&number.to_string()) {
            fail(
                seed,
                case,
                "the query log kept data",
                &sql,
                &format!("shape: {shape:?}"),
            );
        }
    }
}

#[test]
fn scanners_survive_arbitrary_text() {
    let seed = seed();
    let mut rng = Rng(seed ^ 5);
    for _ in 0..cases() * 4 {
        let text = if rng.chance(50) {
            garbage(&mut rng)
        } else {
            // Truncated real statements: every prefix is an unterminated
            // literal, comment or identifier waiting to happen.
            let s = statement(&mut rng, 0);
            let cut = rng.below(s.len() + 1);
            let cut = (0..=cut).rev().find(|&i| s.is_char_boundary(i)).unwrap();
            s[..cut].to_string()
        };
        let _ = crate::sqlsplit::split_statements(&text);
        let _ = crate::store::queries::normalise(&text);
        let _ = may_reindex(&text);
        let _ = strip_comments(&text);
        let _ = is_read_only_one(&text);
    }
}
