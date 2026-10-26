//! TEMPORARY probe — deleted before commit.
use dg::tui::App;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};

fn backups() -> Value {
    json!({
        "backup_root": "/var/lib/dg-backups",
        "stale_after_secs": 300,
        "cadence_secs": 3600,
        "sets": [{
            "timestamp": "20261021T070000Z",
            "databases": ["demo"],
            "has_registry": true,
            "coverage_start": 1_700_000_000,
            "coverage_end": 1_700_000_100
        }],
        "per_database": {
            "demo": {"state": "fresh", "newest_set": "20261021T070000Z", "age_secs": 3600},
            "other": {"state": "stale", "newest_set": "20261001T070000Z", "age_secs": 1728000}
        },
        "last_attempt": {
            "ts": "20261021T070000Z", "started_at": 1_700_000_000, "completed_at": 1_700_000_100,
            "outcome": "ok", "set": "20261021T070000Z", "databases": 2, "step": "publish"
        },
        "last_failure": {
            "ts": "20261020T070000Z", "started_at": 1_699_000_000, "completed_at": 1_699_000_100,
            "outcome": "error", "set": null, "databases": 0, "step": "export"
        },
        "coverage": {"live": 2, "in_newest_set": 1, "missing": ["other"], "registry": true},
        "replica": {
            "target": "s3://dg-sandbox", "bucket": "dg-sandbox", "endpoint": "s3.example.invalid",
            "region": "gra", "retention": "168h", "snapshot_interval": "24h",
            "databases": ["demo", "other"], "registry": true, "sidecar": "active",
            "published_at": 1_700_000_100
        },
        "drill": {
            "outcome": "ok", "drilled_at": 1_700_000_100, "databases": 2,
            "detail": "restored and verified against the live gateway"
        },
        "restore_points": {
            "window_start": "2026-10-15T14:16:09Z", "window_end": "2026-10-20T09:30:00Z",
            "databases": {"demo": {}}, "sidecar": "active"
        },
        "last_restore": {
            "outcome": "ok", "database": "demo", "timestamp": "2026-10-16T00:00:00Z"
        }
    })
}

fn app() -> App {
    let mut app = App::new("http://127.0.0.1:7432".to_string(), "0".repeat(64));
    app.backups = Some(backups());
    app.connected = true;
    app
}

fn screen(app: &mut App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("a test terminal");
    terminal.draw(|frame| dg::tui::draw::draw(frame, app)).expect("draws");
    let buffer = terminal.backend().buffer();
    let mut lines = Vec::new();
    for y in 0..height {
        let line: String = (0..width)
            .map(|x| buffer.cell((x, y)).map(|c| c.symbol().to_string()).unwrap_or_default())
            .collect();
        lines.push(line);
    }
    lines.join("\n")
}

#[test]
fn probe() {
    for (w, h) in [(120u16, 40u16), (191, 50)] {
        let mut a = app();
        if w == 191 {
            let mut per_database = serde_json::Map::new();
            for index in 0..30 {
                per_database.insert(
                    format!("db{index:02}"),
                    json!({"state": "stale", "newest_set": "20261001T070000Z", "age_secs": 1728000}),
                );
            }
            a.backups.as_mut().unwrap()["per_database"] = json!(per_database);
        }
        a.backups.as_mut().unwrap()["deleted"] = json!(3);
        a.on_key(ratatui::crossterm::event::KeyEvent::new(ratatui::crossterm::event::KeyCode::Char('3'), ratatui::crossterm::event::KeyModifiers::NONE));
        let text = screen(&mut a, w, h);
        println!("=== {w}x{h} ===");
        for (i, line) in text.lines().enumerate() {
            println!("{i:2} |{line}|");
        }
    }
}
