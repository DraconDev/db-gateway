//! Painting the view.
//!
//! Every fact on screen comes from the shared view layer, so the
//! terminal view and the CLI show the same thing about the same
//! gateway. This module only decides where the facts go: it draws the
//! sections and tables the view layer built, and it does not read the
//! gateway's answers itself.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    render::{self, Block as ViewBlock, Section, Style as ViewStyle, Table as ViewTable},
    tui::{App, Focus, Mode, Outcome},
    views,
};
use std::time::{Duration, Instant};

/// Where the tab strip and the tab bodies landed on the last frame.
///
/// Every tab has to fit across one line, and a narrow terminal may not
/// have room for all four. Rather than have the mouse handler guess, the
/// drawing code records the exact cells each label occupied and the whole
/// body rectangle, so a click is resolved against what was actually drawn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Geometry {
    /// One `(focus, start column, width)` per drawn tab label, in order.
    pub tabs: Vec<(Focus, u16, u16)>,
    /// The strip line. Clicks on this row are tab clicks.
    pub strip_y: Option<u16>,
    /// The body rectangle of the open tab.
    pub body: Option<Rect>,
}

/// The paint for a view style.
///
/// The colours live in `tui::theme`; this is the one bridge from the view
/// layer's meaning to a terminal style, so a restyle is a theme change and
/// not an edit to every drawing function.
fn paint_style(style: ViewStyle) -> Style {
    crate::tui::theme::paint(style)
}

/// One cell of a view table, as a widget cell.
fn cell_widget(cell: &render::Cell) -> Cell<'static> {
    let (text, style) = cell.parts();
    Cell::from(Span::styled(text.into_owned(), paint_style(style)))
}

/// The widths of a table's columns.
///
/// A column is as wide as its widest cell, so the numbers line up
/// under their headers; when the table is wider than the area, every
/// column shares what is left rather than one of them taking it all.
fn widths(table: &ViewTable, area: u16) -> Vec<Constraint> {
    let columns = table.columns();
    let mut widths: Vec<u16> = columns
        .iter()
        .map(|(name, _)| UnicodeWidthStr::width(name.as_str()) as u16)
        .collect();
    for row in table.rows() {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                let (text, _) = cell.parts();
                *width = (*width).max(UnicodeWidthStr::width(text.as_ref()) as u16);
            }
        }
    }
    // Two cells of padding on each side of a column's text.
    let padded: Vec<u16> = widths.iter().map(|width| width.saturating_add(2)).collect();
    let total: u32 = padded.iter().map(|width| *width as u32).sum();
    if total <= area as u32 {
        padded.into_iter().map(Constraint::Length).collect()
    } else {
        vec![Constraint::Ratio(1, columns.len().max(1) as u32); columns.len()]
    }
}

/// A view table, as a widget.
fn table_widget(table: &ViewTable, area: u16, max_rows: usize) -> Table<'static> {
    let header = Row::new(
        table
            .columns()
            .iter()
            .map(|(name, _)| Cell::from(Span::styled(name.clone(), paint_style(ViewStyle::Header))))
            .collect::<Vec<Cell<'static>>>(),
    );
    let rows = table
        .rows()
        .iter()
        .take(max_rows)
        .map(|row| Row::new(row.iter().map(cell_widget).collect::<Vec<Cell<'static>>>()))
        .collect::<Vec<Row<'static>>>();
    Table::new(rows, widths(table, area)).header(header)
}

/// A section's fields, as lines.
///
/// Only the field blocks: a table is drawn as a table by its own body, since
/// a paragraph cannot align columns. Callers therefore have to ask
/// `section_has_table` before printing a section's heading -- a section whose
/// only block is a table contributes no lines at all, and printing its
/// heading anyway put an empty "per-database" title in the middle of the
/// service dashboard, above nothing.
fn section_lines(section: &Section) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for block in section.blocks() {
        if let ViewBlock::Fields(fields) = block {
            for (key, cell) in fields {
                let (text, style) = cell.parts();
                lines.push(Line::from(vec![
                    Span::styled(format!("{key}:"), paint_style(ViewStyle::Dim)),
                    Span::raw(" "),
                    Span::styled(text.into_owned(), paint_style(style)),
                ]));
            }
        }
    }
    lines
}

/// Whether a section carries anything `section_lines` can draw.
///
/// A section that holds only a table adds no lines, so a heading printed for
/// it would be a title standing over nothing. The body draws the table
/// itself; this is what keeps the heading honest.
fn section_has_lines(section: &Section) -> bool {
    !section_lines(section).is_empty()
}

/// A section's first table, if it has one.
fn section_table(section: &Section) -> Option<&ViewTable> {
    section.blocks().iter().find_map(|block| match block {
        ViewBlock::Table(table) => Some(table),
        ViewBlock::Fields(_) => None,
    })
}

/// A section's title, as a heading.
fn heading(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        title.to_string(),
        crate::tui::theme::heading(),
    ))
}

/// The border of the tab body.
///
/// A tabbed layout has one body, so there is no longer a set of boxes
/// competing for attention: the border is calm and the tab strip carries the
/// focus signal.
///
/// The frame carries no title. The strip one row above already names the tab
/// and what is in it — "1 databases (2)" — and the frame said the same words
/// a second time, one row apart. The strip is the summary of every tab, so
/// it keeps the name and the count; the body shows the answer and lets the
/// strip say what it is.
fn pane(focused: bool) -> Block<'static> {
    let border = if focused {
        crate::tui::theme::border_focused()
    } else {
        crate::tui::theme::border()
    };
    Block::default().borders(Borders::ALL).border_style(border)
}

/// A named box inside a body: the published sets under the backups tab, the
/// rows of the last action. These name something the strip does not, so
/// they keep their title.
fn pane_titled(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(crate::tui::theme::border())
        .title(Span::styled(
            format!(" {title} "),
            crate::tui::theme::tab(),
        ))
}

/// How long ago something happened, as a person reads it.
fn ago(since: Instant) -> String {
    let seconds = since.elapsed().as_secs();
    if seconds < 60 {
        format!("{seconds}s ago")
    } else {
        render::human_duration(seconds)
    }
}

/// Draw the whole view.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .split(area);
    top_bar(frame, rows[0], app);
    body(frame, rows[1], app);
    status_line(frame, rows[2], app);
    match &app.mode {
        Mode::Palette => palette(frame, area, app),
        Mode::Prompt { kind } => {
            let action = crate::tui::actions::action(*kind);
            prompt(
                frame,
                area,
                &format!("{} — {}", action.title, action.hint),
                &app.input,
            )
        }
        Mode::Confirm { phrase, .. } => prompt(
            frame,
            area,
            &format!("type {phrase} to go ahead"),
            &app.input,
        ),
        Mode::Normal => {}
    }
}

/// The bar along the top: which gateway, when it was read, and whether
/// it answered.
fn top_bar(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![
        Span::styled(" dg ", crate::tui::theme::mark()),
        Span::styled(app.server.clone(), crate::tui::theme::top_bar()),
    ];
    if let Some(refreshed) = app.refreshed {
        spans.push(Span::raw(" · read "));
        spans.push(Span::raw(ago(refreshed)));
    }
    spans.push(Span::raw(" · "));
    spans.push(Span::styled(
        if app.connected {
            "connected"
        } else {
            "unreachable"
        },
        Style::default().fg(if app.connected {
            crate::tui::theme::POSITIVE
        } else {
            crate::tui::theme::NEGATIVE
        }),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The tab strip, then the body of the tab that is showing.
///
/// The view shows one tab at a time rather than tiling every answer. A tile
/// layout forces each answer into a fraction of the screen, so a wide table
/// is squeezed and a long list is cut; a tab gives the whole width to the
/// thing being read. The cost is that the other tabs are not visible, and
/// the strip is what pays it: every tab is always named, always numbered, and
/// always one keypress away.
fn body(frame: &mut Frame, area: Rect, app: &mut App) {
    let outcome_height = app
        .outcome
        .as_ref()
        .map(|outcome| outcome_lines(outcome) + 2)
        .unwrap_or(0);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(outcome_height),
    ])
    .split(area);
    let strip = tabs(frame, rows[0], app);
    app.geometry = Geometry {
        tabs: strip,
        strip_y: Some(rows[0].y),
        body: Some(rows[1]),
    };
    match app.focus {
        Focus::Databases => databases_tab(frame, rows[1], app),
        Focus::Service => service_tab(frame, rows[1], app),
        Focus::Backups => backups_tab(frame, rows[1], app),
        Focus::Keys => keys_tab(frame, rows[1], app),
    }
    if let Some(outcome) = &app.outcome {
        outcome_pane(frame, rows[2], outcome);
    }
}

/// Every tab, named, with the active one marked.
///
/// The strip is the answer to "what is in here": an operator who cannot see
/// the backups tab still has to know it exists. Each tab carries its number
/// as well as its name, because the number is what the keyboard shortcut
/// uses, and a hint that names a key nobody can discover is not a hint.
fn tabs(frame: &mut Frame, area: Rect, app: &App) -> Vec<(Focus, u16, u16)> {
    let all = [
        Focus::Databases,
        Focus::Service,
        Focus::Backups,
        Focus::Keys,
    ];
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cells: Vec<(Focus, u16, u16)> = Vec::new();
    let mut used: u16 = 0;
    for (index, focus) in all.iter().enumerate() {
        // What the tab holds, so the strip is a summary and not just a
        // switchboard: the count is the same number the tab's own heading
        // shows.
        let count = match focus {
            Focus::Databases => app.databases().map(|table| table.rows().len()),
            Focus::Keys => app.keys_table().map(|table| table.rows().len()),
            Focus::Service | Focus::Backups => None,
        };
        let label = match count {
            Some(count) => format!(" {} {} ({count}) ", index + 1, focus.title()),
            None => format!(" {} {} ", index + 1, focus.title()),
        };
        let active = *focus == app.focus;
        let style = if active {
            crate::tui::theme::tab_active()
        } else {
            crate::tui::theme::tab()
        };
        // Underlined means active, so the marker survives a monochrome
        // terminal; the caret is a second, plainer signal.
        let text = if active { format!("▸{label}") } else { label };
        // Record the cells this label will occupy, so a click resolves to
        // what is really on screen rather than to a second layout guess.
        // The width is measured from the text as drawn, caret included, so
        // the caret's own cell is part of the hit area rather than a gap.
        let width = text.width() as u16;
        let column = area.x.saturating_add(used);
        if column < area.x.saturating_add(area.width) {
            let visible = width.min(area.x.saturating_add(area.width).saturating_sub(column));
            cells.push((*focus, column, visible));
        }
        used = used.saturating_add(width);
        spans.push(Span::styled(text, style));
        if index + 1 < all.len() {
            spans.push(Span::styled("│", crate::tui::theme::border()));
            used = used.saturating_add(1);
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    cells
}

/// The database list, with each database's traffic.
///
/// The tab says how many databases it holds: a list that silently
/// cuts itself is a visibility lie, and the count is what tells the
/// operator to scroll.
fn databases_tab(frame: &mut Frame, area: Rect, app: &App) {
    match app.databases() {
        Some(table) => {
            let block = pane(app.focus == Focus::Databases);
            let mut state = TableState::default().with_offset(app.scroll.databases as usize);
            frame.render_stateful_widget(
                table_widget(&table, area.width, usize::MAX).block(block),
                area,
                &mut state,
            );
        }
        None => {
            let block = pane(app.focus == Focus::Databases);
            frame.render_widget(Paragraph::new("no answer yet").block(block), area);
        }
    }
}

/// The service dashboard: what the gateway is doing.
///
/// The dashboard is the service's own facts: which version is answering,
/// how long it has been up, how much traffic it has taken, and how its
/// admission limits are set. It ends with a per-database traffic table —
/// and that table is NOT drawn here, because it is the databases tab's
/// answer, drawn there in full with the whole width for it. Drawing it
/// on both tabs made the service tab repeat tab 1: the same rows, with
/// two extra columns, under a heading the operator had already been
/// shown.
///
/// The CLI's dashboard does still carry the table (`dg status` has no
/// other way to show it), so the section is still built; it is simply
/// not the terminal view's job to say it twice.
fn service_tab(frame: &mut Frame, area: Rect, app: &App) {
    let block = pane(app.focus == Focus::Service);
    let mut lines = Vec::new();
    match &app.status {
        Some(status) => {
            for section in views::service_dashboard(status) {
                // A section's fields become lines. A section that is only a
                // table contributes no lines -- it is the per-database
                // breakdown, which belongs to the databases tab.
                if !section_has_lines(&section) {
                    continue;
                }
                // The first section is the tab's own headline, and the
                // strip already says "service": repeating it spends a
                // line saying nothing. The rest are real groups.
                if section.title() != Focus::Service.title() {
                    lines.push(heading(section.title()));
                }
                lines.extend(section_lines(&section));
            }
        }
        None => lines.push(Line::from("no answer yet")),
    }
    let offset = app.scroll.service as usize;
    let lines: Vec<Line<'static>> = lines.into_iter().skip(offset).collect();
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The backup freshness, per database.
fn backups_tab(frame: &mut Frame, area: Rect, app: &App) {
    let block = pane(app.focus == Focus::Backups);
    let Some(backups) = &app.backups else {
        frame.render_widget(Paragraph::new("no answer yet").block(block), area);
        return;
    };
    let sections = views::backups(backups);
    let mut lines = Vec::new();
    let mut table: Option<(String, ViewTable)> = None;
    for section in &sections {
        // The same rule as the service tab: a heading is only drawn when
        // there is something under it.
        if section_has_lines(section) {
            lines.push(heading(section.title()));
            lines.extend(section_lines(section));
        }
        if table.is_none()
            && let Some(found) = section_table(section)
        {
            // The table's title comes from the section that owns it. It used
            // to be hardcoded to "per-database", so the *published sets*
            // table was labelled per-database -- a title that names the wrong
            // thing is worse than no title.
            table = Some((section.title().to_string(), found.clone()));
        }
    }
    // The fields take exactly the lines they need (plus the border) and the
    // table gets the rest: a fixed four-line allowance cut the freshness
    // fields off the moment a gateway reported more than the root.
    let fields_height = lines.len() as u16 + 2;
    let rows =
        Layout::vertical([Constraint::Length(fields_height), Constraint::Min(1)]).split(area);
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), rows[0]);
    if let Some((title, table)) = table {
        let mut state = TableState::default().with_offset(app.scroll.backups as usize);
        frame.render_stateful_widget(
            table_widget(&table, rows[1].width, usize::MAX).block(pane_titled(&title)),
            rows[1],
            &mut state,
        );
    }
}

/// The credentials that exist.
fn keys_tab(frame: &mut Frame, area: Rect, app: &App) {
    match &app.keys {
        Some(keys) => {
            let table = views::keys(keys);
            let title = format!("{} ({})", Focus::Keys.title(), table.rows().len());
            let block = pane(&title, app.focus == Focus::Keys);
            let mut state = TableState::default().with_offset(app.scroll.keys as usize);
            frame.render_stateful_widget(
                table_widget(&table, area.width, usize::MAX).block(block),
                area,
                &mut state,
            );
        }
        None => {
            let block = pane(Focus::Keys.title(), app.focus == Focus::Keys);
            frame.render_widget(Paragraph::new("no answer yet").block(block), area);
        }
    }
}

/// How many lines an outcome takes.
fn outcome_lines(outcome: &Outcome) -> u16 {
    let facts = outcome.facts.len() as u16;
    let table = outcome
        .table
        .as_ref()
        .map(|table| (table.rows().len().min(OUTCOME_ROWS) as u16) + 1)
        .unwrap_or(0);
    1 + facts + table + u16::from(outcome.error.is_some()) + 4
}

/// How many rows of a result the outcome area draws.
///
/// That area is not a report: a result with a thousand rows belongs in
/// the one-shot command that can print them all.
const OUTCOME_ROWS: usize = 10;

/// What the last action did.
fn outcome_pane(frame: &mut Frame, area: Rect, outcome: &Outcome) {
    if area.height == 0 {
        return;
    }
    let mut lines = vec![Line::from(Span::styled(
        outcome.headline.clone(),
        Style::default()
            .fg(if outcome.error.is_some() {
                crate::tui::theme::NEGATIVE
            } else {
                crate::tui::theme::POSITIVE
            })
            .add_modifier(Modifier::BOLD),
    ))];
    for (key, value) in &outcome.facts {
        lines.push(Line::from(vec![
            Span::styled(format!("{key}:"), paint_style(ViewStyle::Dim)),
            Span::raw(" "),
            Span::raw(value.clone()),
        ]));
    }
    if let Some(error) = &outcome.error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            paint_style(ViewStyle::Red),
        )));
    }
    // The two boxes are sized exactly: the headline, its facts and its
    // error take what they need, and the table of rows takes the rest.
    let facts = 1 + outcome.facts.len() as u16 + u16::from(outcome.error.is_some());
    let table_lines = outcome
        .table
        .as_ref()
        .map(|table| (table.rows().len().min(OUTCOME_ROWS) as u16) + 3)
        .unwrap_or(0);
    let rows = Layout::vertical([
        Constraint::Length(facts + 2),
        Constraint::Length(table_lines),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(pane("last action", false)),
        rows[0],
    );
    if let Some(table) = &outcome.table {
        frame.render_widget(
            table_widget(table, rows[1].width, OUTCOME_ROWS).block(pane("rows", false)),
            rows[1],
        );
    }
}

/// The bar along the bottom: what the operator can do, and what went
/// wrong.
///
/// This is the discoverability surface. The palette behind `:` is powerful
/// but invisible -- nobody finds a command they cannot see -- so the bar
/// names the keys that matter, Always in the same place, in the same order,
/// with the key visually distinct from its label. The keys here are the
/// ones an operator needs on every screen; everything else is one `:` away,
/// and the bar says so.
fn status_line(frame: &mut Frame, area: Rect, app: &App) {
    // Key, then what it does. Ordered by how often it is reached for.
    const HINTS: [(&str, &str); 6] = [
        ("1-4", "tabs"),
        ("tab", "next tab"),
        ("↑↓", "scroll"),
        (":", "commands"),
        ("r", "refresh"),
        ("q", "quit"),
    ];
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, (key, label)) in HINTS.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", crate::tui::theme::status()));
        }
        spans.push(Span::styled(*key, crate::tui::theme::key()));
        spans.push(Span::styled(
            format!(" {label}"),
            crate::tui::theme::key_label(),
        ));
    }
    if let Some(error) = &app.error {
        spans.push(Span::styled("  ·  ", crate::tui::theme::status()));
        spans.push(Span::styled(error.clone(), paint_style(ViewStyle::Red)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The list of actions, over the dashboard.
fn palette(frame: &mut Frame, area: Rect, app: &App) {
    let actions = app.filtered_actions();
    let height = (actions.len() as u16 + 4).clamp(5, area.height.saturating_sub(2));
    let width = area.width.saturating_sub(6).clamp(30, 100);
    let popup = centered(area, width, height);
    let mut lines = Vec::new();
    for (index, action) in actions.iter().enumerate() {
        let selected = index == app.highlighted;
        let style = if selected {
            crate::tui::theme::selection()
        } else {
            crate::tui::theme::paint(ViewStyle::Plain)
        };
        lines.push(Line::from(vec![
            Span::styled(if selected { "▸ " } else { "  " }, style),
            Span::styled(format!("{:<16}", action.title), style),
            Span::raw(action.hint.to_string()),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from("no action matches"));
    }
    let title = if app.filter.is_empty() {
        " actions ".to_string()
    } else {
        format!(" actions: {} ", app.filter)
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(crate::tui::theme::border_focused())
                .title(Span::styled(title, crate::tui::theme::heading())),
        ),
        popup,
    );
}

/// A prompt, over the dashboard.
fn prompt(frame: &mut Frame, area: Rect, label: &str, input: &str) {
    let width = area.width.saturating_sub(6).clamp(30, 100);
    let popup = centered(area, width, 3);
    let line = Line::from(vec![
        Span::styled(format!("{label}: "), crate::tui::theme::heading()),
        Span::styled(
            input.to_string(),
            crate::tui::theme::paint(ViewStyle::Plain),
        ),
        Span::styled("▏", crate::tui::theme::key()),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(crate::tui::theme::border_focused()),
        ),
        popup,
    );
}

/// A box in the middle of an area.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// How long the view has been open, for the tests.
#[allow(dead_code)]
fn _elapsed(since: Instant) -> Duration {
    since.elapsed()
}
