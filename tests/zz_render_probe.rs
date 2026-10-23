#[test]
fn show_the_screen() {
    let mut app = dg::tui::App::new("http://127.0.0.1:7432".into(), "0".repeat(64));
    app.status = Some(serde_json::json!({
        "version":"0.2.0","uptime_secs":743040,
        "admission":{"acquire_timeout_ms":500,"read_permits":8,"write_permits":4},
        "requests":{"total":3218,"capacity_exceeded":0,"query_deadline_exceeded":0,
            "internal_error":0,"recent_per_second":[1,2,3,4,5],"recent_span_secs":5,
            "wait_p50_us":400,"wait_p95_us":600,"wait_samples":9},
        "database_counts":{"live":2,"tombstoned":0},
        "databases":{"verify_probe":{"rows_read":89,"rows_written":0,"queue_waits":0,
            "queue_wait_max_us":0,"queue_wait_p95_us":0,"queue_timeouts":0,"last_write":null},
          "billing_dev_pilot":{"rows_read":1204,"rows_written":88,"queue_waits":3,
            "queue_wait_max_us":1200,"queue_wait_p95_us":900,"queue_timeouts":0,"last_write":1792752128}}
    }));
    app.list = Some(serde_json::json!({"databases":["billing_dev_pilot","verify_probe"]}));
    app.connected = true;
    app.focus = dg::tui::Focus::Service;
    let backend = ratatui::backend::TestBackend::new(110, 30);
    let mut t = ratatui::Terminal::new(backend).unwrap();
    t.draw(|f| dg::tui::draw::draw(f, &app)).unwrap();
    let b = t.backend().buffer();
    let mut out = String::new();
    for y in 0..30 {
        for x in 0..110 {
            out.push_str(b.cell((x,y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        out.push('\n');
    }
    println!("{out}");
}
