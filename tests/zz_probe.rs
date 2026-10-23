#[test]
fn probe() {
    let mut app = dg::tui::App::new("http://127.0.0.1:7432".into(), "0".repeat(64));
    app.backups = Some(serde_json::json!({
        "backup_root":"/var/lib/dg-backups","stale_after_secs":86400,
        "sets":[{"timestamp":"20261023T101707Z","databases":["a_db"],"has_registry":true,"coverage_start":1792746000}],
        "per_database":{"a_db":{"state":"fresh","newest_set":"20261023T101707Z","age_secs":3600}}
    }));
    app.connected = true;
    app.focus = dg::tui::Focus::Backups;
    let backend = ratatui::backend::TestBackend::new(100, 20);
    let mut t = ratatui::Terminal::new(backend).unwrap();
    t.draw(|f| dg::tui::draw::draw(f, &app)).unwrap();
    let b = t.backend().buffer();
    for y in 0..20 {
        let line: String = (0..100).map(|x| b.cell((x,y)).map(|c| c.symbol()).unwrap_or(" ")).collect();
        println!("{}", line.trim_end());
    }
}
