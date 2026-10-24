use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde_json::json;

fn status() -> serde_json::Value {
    json!({
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
    })
}

#[test]
fn probe() {
    let list = json!({"databases": ["billing_dev_pilot", "verify_probe"]});
    let mut app = dg::tui::App::new("http://127.0.0.1:7432".into(), "0".repeat(64));
    app.status = Some(status());
    app.list = Some(list.clone());
    app.connected = true;
    app.focus = dg::tui::Focus::Databases;
    let mut term = Terminal::new(TestBackend::new(96, 14)).unwrap();
    term.draw(|f| dg::tui::draw::draw(f, &mut app)).unwrap();
    let buf = term.backend().buffer();
    println!("=== DATABASES TAB (short terminal) ===");
    for y in 0..14 {
        let line: String = (0..96)
            .map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()).unwrap_or_default())
            .collect();
        println!("{}", line.trim_end());
    }
    app.focus = dg::tui::Focus::Service;
    let mut term = Terminal::new(TestBackend::new(96, 22)).unwrap();
    term.draw(|f| dg::tui::draw::draw(f, &mut app)).unwrap();
    let buf = term.backend().buffer();
    println!("=== SERVICE TAB ===");
    for y in 0..22 {
        let line: String = (0..96)
            .map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()).unwrap_or_default())
            .collect();
        println!("{}", line.trim_end());
    }
}
