use dg::tui::App;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};

fn status() -> Value {
    json!({
        "version": "0.2.0",
        "uptime_secs": 3600,
        "admission": {"acquire_timeout_ms": 250, "read_permits": 4, "write_permits": 2},
        "requests": {
            "total": 120, "capacity_exceeded": 1, "query_deadline_exceeded": 0,
            "internal_error": 0, "recent_per_second": [1,2,3], "recent_span_secs": 3,
            "wait_p50_us": 500, "wait_p95_us": 1000, "wait_samples": 4
        },
        "database_counts": {"live": 2, "tombstoned": 1},
        "databases": {
            "billing_dev_pilot": {"rows_read": 1200, "rows_written": 44, "queue_waits": 2,
                "queue_wait_ms": 3, "queue_wait_max_us": 900000, "queue_wait_p95_us": 1000,
                "queue_timeouts": 0, "last_write": 1700000000},
            "verify_probe": {"rows_read": 1, "rows_written": 0, "queue_waits": 0,
                "queue_wait_ms": 0, "queue_wait_max_us": 0, "queue_wait_p95_us": 0,
                "queue_timeouts": 0, "last_write": null}
        },
        "deployment": {
            "host": "micro2",
            "listen": "127.0.0.1:7432",
            "data_dir": "/home/ubuntu/dg/data",
            "backup_root": "/home/ubuntu/dg/dg-backups",
            "databases": {
                "billing_dev_pilot": {"file": "/home/ubuntu/dg/data/billing_dev_pilot.sqlite",
                    "exists": true, "size_bytes": 40960, "tombstoned": false},
                "verify_probe": {"file": "/home/ubuntu/dg/data/verify_probe.sqlite",
                    "exists": true, "size_bytes": 12288, "tombstoned": false},
                "old_one": {"file": "/home/ubuntu/dg/data/old_one.sqlite",
                    "exists": false, "size_bytes": 0, "tombstoned": true}
            }
        }
    })
}

fn app() -> App {
    let mut app = App::new("http://127.0.0.1:7432".to_string(), "0".repeat(64));
    app.status = Some(status());
    app.list = Some(json!({"databases": ["billing_dev_pilot", "verify_probe"]}));
    app.connected = true;
    app
}

fn screen(app: &mut App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|f| dg::tui::draw::draw(f, app)).expect("draw");
    let buffer = terminal.backend().buffer();
    let mut lines = Vec::new();
    for y in 0..height {
        let line: String = (0..width)
            .map(|x| buffer.cell((x, y)).map(|c| c.symbol().to_string()).unwrap_or_default())
            .collect();
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

#[test]
fn probe() {
    let mut app = app();
    println!("=== DATABASES TAB (191x30) ===\n{}", screen(&mut app, 191, 30));
    app.on_key(ratatui::crossterm::event::KeyEvent::new(
        ratatui::crossterm::event::KeyCode::Char('2'),
        ratatui::crossterm::event::KeyModifiers::NONE));
    println!("=== SERVICE TAB (191x30) ===\n{}", screen(&mut app, 191, 30));
    let mut old = app();
    old.status = Some(json!({"version": "0.1.0", "uptime_secs": 10}));
    old.list = Some(json!({"databases": ["billing_dev_pilot"]}));
    println!("=== OLD GATEWAY, DATABASES TAB ===\n{}", screen(&mut old, 191, 16));
    old.on_key(ratatui::crossterm::event::KeyEvent::new(
        ratatui::crossterm::event::KeyCode::Char('2'),
        ratatui::crossterm::event::KeyModifiers::NONE));
    println!("=== OLD GATEWAY, SERVICE TAB ===\n{}", screen(&mut old, 191, 16));
}
