//! Throwaway: render the TUI with a database page open and print the screen.
use dg::tui::{App, DatabasePage};
use ratatui::{Terminal, backend::TestBackend};

fn main() {
    let structure = serde_json::json!({
        "database": "inventory",
        "tables": [
            {"name": "products", "rows": 42, "columns": [{"name":"sku","type":"TEXT"}]},
            {"name": "stock_moves", "rows": 900, "columns": [{"name":"sku","type":"TEXT"}]}
        ]
    });
    let mut tables = dg::views::structures(&structure, None);
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    let mut app = App::new("http://127.0.0.1:7432".to_string(), "0".repeat(64));
    app.page = Some(DatabasePage {
        database: "inventory".to_string(),
        tables,
        selected: 0,
        list_scroll: 0,
    });
    let (w, h) = (78u16, 18u16);
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
    terminal.draw(|f| dg::tui::draw::draw(f, &mut app)).expect("draw");
    let buf = terminal.backend().buffer();
    for y in 0..h {
        let line: String = (0..w)
            .map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()).unwrap_or_default())
            .collect();
        println!("{line}");
    }
}
