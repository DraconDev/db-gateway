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

#[test]
fn probe_lines() {
    let sections = dg::views::backups(&backups());
    let mut n = 0;
    for s in &sections {
        let has_table = s.blocks().iter().any(|b| matches!(b, dg::render::ViewBlock::Table(_)));
        let mut lines = 0;
        for b in s.blocks() {
            if let dg::render::ViewBlock::Fields(fields) = b {
                lines += fields.len();
            }
        }
        println!("section {:?}: field_blocks={} has_table={}", s.title(), lines, has_table);
        n += lines;
        if lines > 0 {
            n += 1; // heading
        }
    }
    println!("TOTAL field lines (excl frame): {n}");
}
