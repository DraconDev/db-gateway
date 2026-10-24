use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde_json::json;

#[test]
fn probe() {
    let backups = json!({
        "backup_root": "/var/lib/dg-backups", "stale_after_secs": 86400,
        "sets": [{"timestamp": "20261021T070000Z", "databases": ["demo"], "has_registry": true,
                  "coverage_start": 1_700_000_000, "coverage_end": 1_700_000_100}],
        "per_database": {"demo": {"state": "fresh", "newest_set": "20261021T070000Z", "age_secs": 3600}}
    });
    let status = json!({
        "version": "0.2.0", "uptime_secs": 3600,
        "requests": {"total": 120, "recent_per_second": [0,0], "capacity_exceeded": 0,
                     "query_deadline_exceeded": 0, "internal_error": 0},
        "admission": {"acquire_timeout_ms": 500, "read_permits": 8, "write_permits": 4,
                      "wait_p50_us": 1000, "wait_p95_us": 1000},
        "databases": {"demo": {"rows_read": 10, "rows_written": 1, "queue_waits": 2,
                               "queue_wait_p95_us": 1000, "queue_wait_max_us": 900000, "last_write": null}}
    });
    let mut app = dg::tui::App::new("http://127.0.0.1:7432".into(), "0".repeat(64));
    app.status = Some(status);
    app.backups = Some(backups);
    app.list = Some(json!({"databases": ["demo"]}));
    app.keys = Some(json!({"keys": [{"name": "owner", "role": "owner", "databases": []}]}));
    app.connected = true;
    app.focus = dg::tui::Focus::Backups;
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    term.draw(|f| dg::tui::draw::draw(f, &mut app)).unwrap();
    let buf = term.backend().buffer();
    for y in 0..40 {
        let line: String = (0..120)
            .map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()).unwrap_or_default())
            .collect();
        if line.contains("fresh") {
            println!("HIT row {y}: {}", line.trim_end());
        }
    }
    println!("no 'fresh' found" );
}
