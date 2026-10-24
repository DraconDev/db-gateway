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
    let mut app = dg::tui::App::new("http://127.0.0.1:7432".into(), "0".repeat(64));
    app.backups = Some(backups);
    app.connected = true;
    app.focus = dg::tui::Focus::Backups;
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    term.draw(|f| dg::tui::draw::draw(f, &mut app)).unwrap();
    let buf = term.backend().buffer();
    for y in 0..40 {
        let line: String = (0..120)
            .map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()).unwrap_or_default())
            .collect();
        println!("{:>2}|{}", y, line.trim_end());
    }
}
