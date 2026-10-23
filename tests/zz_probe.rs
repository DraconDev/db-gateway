#[test]
fn probe() {
    for (tab, name) in [(dg::tui::Focus::Service,"SERVICE"),(dg::tui::Focus::Backups,"BACKUPS"),(dg::tui::Focus::Keys,"KEYS")] {
        let mut app = dg::tui::App::new("http://127.0.0.1:7432".into(), "0".repeat(64));
        app.status = Some(serde_json::json!({
            "version":"0.2.0","uptime_secs":743040,
            "admission":{"acquire_timeout_ms":500,"read_permits":8,"write_permits":4},
            "requests":{"total":3218,"capacity_exceeded":0,"query_deadline_exceeded":0,
                "internal_error":0,"recent_per_second":[1,2,3,4,5],"recent_span_secs":5,
                "wait_p50_us":400,"wait_p95_us":600,"wait_samples":9},
            "database_counts":{"live":2,"tombstoned":0},
            "databases":{"a_db":{"rows_read":89,"rows_written":2,"queue_waits":1,
                "queue_wait_max_us":900,"queue_wait_p95_us":700,"queue_timeouts":0,"last_write":1792752128}}
        }));
        app.backups = Some(serde_json::json!({
            "backup_root":"/var/lib/dg-backups","stale_after_secs":86400,
            "sets":[{"timestamp":"20261023T101707Z","databases":["a_db"],"has_registry":true,"coverage_start":1792746000}],
            "per_database":{"a_db":{"state":"fresh","newest_set":"20261023T101707Z","age_secs":3600}}
        }));
        app.keys = Some(serde_json::json!({"keys":[
            {"name":"owner","role":"owner","databases":[]},
            {"name":"pilot","role":"database_read_write","databases":["a_db"]}]}));
        app.list = Some(serde_json::json!({"databases":["a_db"]}));
        app.connected = true;
        app.focus = tab;
        let backend = ratatui::backend::TestBackend::new(100, 22);
        let mut t = ratatui::Terminal::new(backend).unwrap();
        t.draw(|f| dg::tui::draw::draw(f, &app)).unwrap();
        let b = t.backend().buffer();
        println!("=== {name} ===");
        for y in 0..22 {
            let line: String = (0..100).map(|x| b.cell((x,y)).map(|c| c.symbol()).unwrap_or(" ")).collect();
            println!("{}", line.trim_end());
        }
    }
}
