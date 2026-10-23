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
    style::{Color, Modifier, Style},
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

/// The colour of a view style.
fn paint_style(style: ViewStyle) -> Style {
    match style {
        ViewStyle::Plain => Style::default(),
        ViewStyle::Dim => Style::default().fg(Color::DarkGray),
        ViewStyle::Bold => Style::default().add_modifier(Modifier::BOLD),
        ViewStyle::Header => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        ViewStyle::Red => Style::default().fg(Color::Red),
        ViewStyle::Green => Style::default().fg(Color::Green),
        ViewStyle::Yellow => Style::default().fg(Color::Yellow),
        ViewStyle::Cyan => Style::default().fg(Color::Cyan),
    }
}

/// One cell of a view table, as a widget cell.
fn cell_widget(cell: &render::Cell) -> Cell<'static> {
    let (text, style) = cell.parts();
    Cell::from(Span::styled(text.into_owned(), paint_style(style)))
}

/// The widths of a table's columns.
///
/// A column is as wide as its widest cell, so the numbers line up
/// under their headers; when the table is wider than the pane, every
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
/// Only the field blocks: a table is drawn as a table by its own pane, since
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
/// it would be a title standing over nothing. The pane draws the table
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
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

/// The border of a pane, lit when the pane has the keyboard.
fn pane(title: &str, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(Span::styled(
            format!(" {title} "),
            if focused {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            },
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
pub fn draw(frame: &mut Frame, app: &App) {
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
        Span::styled(
            " dg ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(app.server.clone()),
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
            Color::Green
        } else {
            Color::Red
        }),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The panes below the top bar.
fn body(frame: &mut Frame, area: Rect, app: &App) {
    let outcome_height = app
        .outcome
        .as_ref()
        .map(|outcome| outcome_lines(outcome) + 2)
        .unwrap_or(0);
    let rows =
        Layout::vertical([Constraint::Min(4), Constraint::Length(outcome_height)]).split(area);
    let columns =
        Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)]).split(rows[0]);
    databases_pane(frame, columns[0], app);
    let right = Layout::vertical([
        Constraint::Percentage(50),
        Constraint::Percentage(28),
        Constraint::Percentage(22),
    ])
    .split(columns[1]);
    service_pane(frame, right[0], app);
    backups_pane(frame, right[1], app);
    keys_pane(frame, right[2], app);
    if let Some(outcome) = &app.outcome {
        outcome_pane(frame, rows[1], outcome);
    }
}

/// The database list, with each database's traffic.
///
/// The pane says how many databases it holds: a list that silently
/// cuts itself is a visibility lie, and the count is what tells the
/// operator to scroll.
fn databases_pane(frame: &mut Frame, area: Rect, app: &App) {
    match app.databases() {
        Some(table) => {
            let title = format!("{} ({})", Focus::Databases.title(), table.rows().len());
            let block = pane(&title, app.focus == Focus::Databases);
            let mut state = TableState::default().with_offset(app.scroll.databases as usize);
            frame.render_stateful_widget(
                table_widget(&table, area.width, usize::MAX).block(block),
                area,
                &mut state,
            );
        }
        None => {
            let block = pane(Focus::Databases.title(), app.focus == Focus::Databases);
            frame.render_widget(Paragraph::new("no answer yet").block(block), area);
        }
    }
}

/// The service dashboard: what the gateway is doing.
fn service_pane(frame: &mut Frame, area: Rect, app: &App) {
    let block = pane(Focus::Service.title(), app.focus == Focus::Service);
    let mut lines = Vec::new();
    match &app.status {
        Some(status) => {
            for section in views::service_dashboard(status) {
                // A section that is only a table would otherwise print a
                // heading with nothing under it.
                if !section_has_lines(&section) {
                    continue;
                }
                lines.push(heading(section.title()));
                lines.extend(section_lines(&section));
            }
        }
        None => lines.push(Line::from("no answer yet")),
    }
    // The pane scrolls, so a long dashboard does not lose its tail.
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
fn backups_pane(frame: &mut Frame, area: Rect, app: &App) {
    let block = pane(Focus::Backups.title(), app.focus == Focus::Backups);
    let Some(backups) = &app.backups else {
        frame.render_widget(Paragraph::new("no answer yet").block(block), area);
        return;
    };
    let sections = views::backups(backups);
    let mut lines = Vec::new();
    let mut table = None;
    for section in &sections {
        // The same rule as the service pane: a heading is only drawn when
        // there is something under it.
        if section_has_lines(section) {
            lines.push(heading(section.title()));
            lines.extend(section_lines(section));
        }
        if table.is_none() {
            table = section_table(section);
        }
    }
    let rows = Layout::vertical([Constraint::Length(4), Constraint::Min(1)]).split(area);
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), rows[0]);
    if let Some(table) = table {
        let mut state = TableState::default().with_offset(app.scroll.backups as usize);
        frame.render_stateful_widget(
            table_widget(table, rows[1].width, usize::MAX).block(pane("per-database", false)),
            rows[1],
            &mut state,
        );
    }
}

/// The credentials that exist.
fn keys_pane(frame: &mut Frame, area: Rect, app: &App) {
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

/// How many rows of a result the outcome pane draws.
///
/// A pane is not a report: a result with a thousand rows belongs in
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
                Color::Red
            } else {
                Color::Green
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
fn status_line(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(
        " : ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::raw("actions"));
    spans.push(Span::raw("  r "));
    spans.push(Span::raw("refresh"));
    spans.push(Span::raw("  tab "));
    spans.push(Span::raw("pane"));
    spans.push(Span::raw("  q "));
    spans.push(Span::raw("quit"));
    if let Some(error) = &app.error {
        spans.push(Span::raw("  ·  "));
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
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
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
                .border_style(Style::default().fg(Color::Cyan))
                .title(Span::styled(
                    title,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
        ),
        popup,
    );
}

/// A prompt, over the dashboard.
fn prompt(frame: &mut Frame, area: Rect, label: &str, input: &str) {
    let width = area.width.saturating_sub(6).clamp(30, 100);
    let popup = centered(area, width, 3);
    let line = Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(Color::Cyan)),
        Span::raw(input.to_string()),
        Span::styled("▏", Style::default().fg(Color::Cyan)),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
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
