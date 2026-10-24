use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde_json::json;

#[test]
fn probe() {
    let status = json!({
        "version": "0.1.0", "uptime_secs": 743040,
        "requests": {"total": 53120, "recent_per_second": [0,0,0], "capacity_exceeded": 0,
                     "query_deadline_exceeded": 0, "internal_error": 0},
        "admission": {"acquire_timeout_ms": 500, "read_permits": 8, "write_permits": 4,
                      "wait_p50_us": null, "wait_p95_us": null},
        "databases": {
            "billing_dev_pilot": {"rows_read": 7, "rows_written": 1, "queue_waits": 11,
                                  "queue_wait_p95_us": null, "queue_wait_max_us": null, "last_write": null},
            "verify_probe": {"rows_read": 89, "rows_written": 0, "queue_waits": 56,
                             "queue_wait_p95_us": null, "queue_wait_max_us": null, "last_write": null}
        }
    });
    let backups = json!({
        "backup_root": "/var/lib/dg-backups", "stale_after_secs": 86400,
        "sets": [{"timestamp": "20261021T070000Z", "databases": ["billing_dev_pilot"], "has_registry": true,
                  "coverage_start": 1_700_000_000, "coverage_end": 1_700_000_100}],
        "per_database": {"billing_dev_pilot": {"state": "fresh", "newest_set": "20261021T070000Z", "age_secs": 3600}}
    });
    let keys = json!({"keys": [
        {"name": "owner", "role": "owner", "databases": []},
        {"name": "robot", "role": "database_read_only", "databases": ["billing_dev_pilot"]}
    ]});
    let list = json!({"databases": ["billing_dev_pilot", "verify_probe"]});
    for (focus, name) in [
        (dg::tui::Focus::Backups, "BACKUPS TAB"),
        (dg::tui::Focus::Keys, "KEYS TAB"),
    ] {
        let mut app = dg::tui::App::new("http://127.0.0.1:7432".into(), "0".repeat(64));
        app.status = Some(status.clone());
        app.list = Some(list.clone());
        app.backups = Some(backups.clone());
        app.keys = Some(keys.clone());
        app.connected = true;
        app.focus = focus;
        let mut term = Terminal::new(TestBackend::new(96, 16)).unwrap();
        term.draw(|f| dg::tui::draw::draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        println!("=== {name} ===");
        for y in 0..16 {
            let line: String = (0..96)
                .map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()).unwrap_or_default())
                .collect();
            println!("{}", line.trim_end());
        }
    }
}
