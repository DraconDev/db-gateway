//! The Rust shadow (`dg_client::shadow`, feature `shadow`) against a real
//! `dg serve`: writes mirrored in order, divergences reported and never
//! raised, the queue bound honoured, and flush a real barrier.

mod common;

use dg_client::{
    Gateway, Statement, Value,
    shadow::{DivergenceKind, Shadow, ShadowOptions},
};
use std::sync::{Arc, Mutex};

type Seen = Arc<Mutex<Vec<(DivergenceKind, String)>>>;

fn recorder() -> (ShadowOptions, Seen) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let options = ShadowOptions {
        max_queue: 1000,
        on_divergence: Arc::new(move |d| sink.lock().unwrap().push((d.kind, d.sql))),
    };
    (options, seen)
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_are_mirrored_in_order_and_divergences_are_reported() {
    let fleet = common::Fleet::boot();
    let owner = Gateway::from_key_file(&fleet.cli.server, &fleet.cli.key_file).unwrap();
    for name in ["primary", "copy"] {
        owner.create_database(name).await.unwrap();
    }
    let primary = owner.database("primary").unwrap();
    let copy = owner.database("copy").unwrap();
    let (options, seen) = recorder();
    let shadow = Shadow::spawn(copy.clone(), options);

    // What an application does: write to its primary, then mirror the
    // write with the primary's own row count.
    let write = |sql: &str, args: Vec<Value>| {
        let (primary, shadow, sql) = (primary.clone(), shadow.clone(), sql.to_string());
        async move {
            let result = primary.execute(&sql, args.clone()).await.unwrap();
            shadow.mirror(
                vec![Statement::new(sql).args(args)],
                Some(vec![result.rows_affected]),
            );
        }
    };
    write("CREATE TABLE q (id INTEGER PRIMARY KEY, to_address TEXT, attempts INTEGER, body BLOB, score REAL)", vec![]).await;
    for i in 0..200 {
        write(
            "INSERT INTO q (to_address, attempts, body, score) VALUES (?, ?, ?, ?)",
            vec![
                Value::Text(format!("user{i}@example.com")),
                Value::Integer(i64::MAX - i),
                Value::Blob(vec![i as u8, 0, 255]),
                Value::Real(-2006.7142857142858 + i as f64),
            ],
        )
        .await;
    }
    // Order matters: each UPDATE depends on the one before it.
    for _ in 0..20 {
        write(
            "UPDATE q SET attempts = attempts - 1 WHERE id % 2 = 0",
            vec![],
        )
        .await;
    }
    write("DELETE FROM q WHERE id > 190", vec![]).await;
    write("ALTER TABLE q ADD COLUMN note TEXT DEFAULT 'n'", vec![]).await;
    // An atomic multi-statement write.
    let batch = vec![
        Statement::new("UPDATE q SET note = ? WHERE id = 1")
            .args(vec![Value::Text("first".into())]),
        Statement::new("DELETE FROM q WHERE id = 2"),
    ];
    let results = primary.batch(batch.clone()).await.unwrap();
    shadow.mirror(
        batch,
        Some(results.iter().map(|r| r.rows_affected).collect()),
    );
    shadow.flush().await;

    assert_eq!(*seen.lock().unwrap(), vec![], "no divergence expected");
    assert_eq!(shadow.stats().mirrored, 1 + 200 + 20 + 1 + 1 + 2);
    let dump = "SELECT * FROM q ORDER BY id";
    assert_eq!(
        primary.execute(dump, vec![]).await.unwrap().rows,
        copy.execute(dump, vec![]).await.unwrap().rows,
        "the copy differs from the primary"
    );

    // A write dg refuses: reported, and the caller never sees an error.
    shadow.mirror(vec![Statement::new("INSERT INTO missing VALUES (1)")], None);
    // A row count that disagrees: the copy has drifted.
    copy.execute("DELETE FROM q WHERE id = 3", vec![])
        .await
        .unwrap();
    shadow.mirror(
        vec![Statement::new("DELETE FROM q WHERE id = 3")],
        Some(vec![1]),
    );
    // Time-dependent SQL: mirrored, and flagged.
    shadow.mirror(
        vec![Statement::new(
            "UPDATE q SET note = datetime('now') WHERE id = 4",
        )],
        Some(vec![1]),
    );
    shadow.flush().await;
    let kinds: Vec<_> = seen.lock().unwrap().iter().map(|(k, _)| *k).collect();
    assert_eq!(
        kinds,
        // Time-dependence is visible in the text, so it is reported the
        // moment `mirror` is called; the other two only once dg answers.
        vec![
            DivergenceKind::Nondeterministic,
            DivergenceKind::ShadowError,
            DivergenceKind::RowsAffectedMismatch,
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_queue_drops_and_reports_instead_of_blocking() {
    let fleet = common::Fleet::boot();
    let owner = Gateway::from_key_file(&fleet.cli.server, &fleet.cli.key_file).unwrap();
    owner.create_database("tiny").await.unwrap();
    let copy = owner.database("tiny").unwrap();
    copy.execute("CREATE TABLE t (x)", vec![]).await.unwrap();
    let (mut options, seen) = recorder();
    options.max_queue = 2;
    let shadow = Shadow::spawn(copy.clone(), options);
    let started = std::time::Instant::now();
    for i in 0..50 {
        shadow.mirror(
            vec![Statement::new("INSERT INTO t VALUES (?)").args(vec![Value::Integer(i)])],
            None,
        );
    }
    // 50 mirror calls against a two-slot queue return at once: the
    // application is never made to wait on dg.
    assert!(started.elapsed() < std::time::Duration::from_millis(200));
    shadow.flush().await;
    let stats = shadow.stats();
    assert!(stats.dropped > 0, "{stats:?}");
    assert_eq!(stats.mirrored + stats.dropped, 50, "{stats:?}");
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|(k, _)| *k == DivergenceKind::Dropped)
    );
    let rows = copy
        .execute("SELECT count(*) FROM t", vec![])
        .await
        .unwrap()
        .rows;
    assert_eq!(rows, vec![vec![Value::Integer(stats.mirrored as i64)]]);
}
