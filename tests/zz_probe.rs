use dg::tui::App;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};

fn backups() -> Value {
    json!({
        "backup_root": "/var/lib/dg-backups",
        "stale_after_secs": 86400,
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
            "ts": "20261021T070000Z",
            "started_at": 1_700_000_000,
            "completed_at": 1_700_000_100,
            "outcome": "ok",
            "set": "20261021T070000Z",
            "databases": 2,
            "step": "publish"
        },
        "last_failure": {
            "ts": "20261020T070000Z",
            "started_at": 1_699_000_000,
            "completed_at": 1_699_000_100,
            "outcome": "error",
            "set": null,
            "databases": 0,
            "step": "export"
        },
        "coverage": {"live": 2, "in_newest_set": 1, "missing": ["other"], "registry": true},
        "replica": {
            "target": "s3://dg-sandbox",
            "bucket": "dg-sandbox",
            "endpoint": "s3.example.invalid",
            "region": "gra",
            "retention": "168h",
            "snapshot_interval": "24h",
            "databases": ["demo", "other"],
            "registry": true,
            "sidecar": "active",
            "published_at": 1_700_000_100
        },
        "drill": {
            "drilled_at": 1_699_000_000,
            "outcome": "ok",
            "databases": 2,
            "detail": "restored and verified against the live gateway"
        },
        "restore_points": {
            "published_at": 1_700_000_200,
            "window_start": "2026-10-15T14:16:08Z",
            "window_end": "2026-10-20T09:30:41Z",
            "sidecar": "active",
            "databases": {
                "demo": {"oldest": "2026-10-15T14:16:08Z", "newest": "2026-10-16T07:30:03Z", "files": 3, "bytes": 1200},
                "other": {"oldest": "2026-10-15T14:16:08Z", "newest": "2026-10-16T07:30:03Z", "files": 3, "bytes": 1200}
            }
        },
        "last_restore": {
            "restored_at": 1_700_000_300,
            "database": "demo",
            "timestamp": "2026-10-16T00:00:00Z",
            "outcome": "ok",
            "detail": "restored and verified",
            "tables": 3,
            "rows": 120,
            "size_bytes": 40960,
            "integrity": "ok",
            "path": "/var/lib/dg-restores/20261016000000/demo.sqlite"
        }
    })
}

fn screen(app: &mut App, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| dg::tui::draw::draw(f, app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    (0..h)
        .map(|y| {
            (0..w)
                .map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().to_string())
                        .unwrap_or_default()
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn key(code: ratatui::crossterm::event::KeyCode) -> ratatui::crossterm::event::KeyEvent {
    ratatui::crossterm::event::KeyEvent::new(code, ratatui::crossterm::event::KeyModifiers::NONE)
}

fn mkapp30() -> App {
    let mut app = App::new("http://127.0.0.1:7432".to_string(), "0".repeat(64));
    let mut value = backups();
    let mut per_database = serde_json::Map::new();
    for index in 0..30 {
        per_database.insert(
            format!("db{index:02}"),
            json!({"state": "stale", "newest_set": "20261001T070000Z", "age_secs": 1728000}),
        );
    }
    value["per_database"] = json!(per_database);
    app.backups = Some(value);
    app.connected = true;
    app
}

fn open3(app: &mut App) {
    app.on_key(key(ratatui::crossterm::event::KeyCode::Char('3')));
}

#[test]
fn probe30() {
    let mut a = mkapp30();
    open3(&mut a);
    let t = screen(&mut a, 191, 50);
    let rows: Vec<&str> = t.lines().filter(|l| l.contains("db")).collect();
    println!("db-row lines visible initially: {}", rows.len());
    println!("dg-sandbox visible: {}", t.contains("dg-sandbox"));
    // scroll in steps and report when each fact appears
    for downs in [8u32, 10, 12, 19, 30] {
        let mut b = mkapp30();
        open3(&mut b);
        for _ in 0..downs {
            b.on_key(key(ratatui::crossterm::event::KeyCode::Down));
        }
        let s = screen(&mut b, 191, 50);
        println!(
            "downs={downs} sandbox={} 168h={} restored={} window={}",
            s.contains("dg-sandbox"),
            s.contains("168h"),
            s.contains("restored and verified"),
            s.contains("2026-10-15 14:16 UTC"),
        );
    }
    // tall screen: everything visible?
    let mut c = mkapp30();
    open3(&mut c);
    let tall = screen(&mut c, 191, 90);
    println!(
        "tall90 all30rows={} sandbox={}",
        (0..30).all(|i| tall.contains(&format!("db{i:02}"))),
        tall.contains("dg-sandbox")
    );
}
