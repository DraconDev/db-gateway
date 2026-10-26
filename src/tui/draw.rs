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
use serde_json::Value;
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

/// A table inside a frame, with the view's footer under it as a caption.
///
/// The shared view layer writes a footer because the columns need one: the
/// traffic table says what "wait p95" and "wait max" mean, and the CLI
/// prints it under the rows. The view used to drop it, so a column whose
/// meaning was on screen nowhere was on screen.
///
/// The caption goes directly under the rows, as the CLI prints it. An
/// explanation pinned to the foot of a tall frame is not next to what it
/// explains, and "wait p95" is read next to the numbers, not a screen away.
fn framed_table(
    frame: &mut Frame,
    area: Rect,
    block: Block<'static>,
    table: &ViewTable,
    max_rows: usize,
    state: &mut TableState,
) {
    let caption = table.footer_text().map(str::to_string);
    let inner = block.inner(area);
    // The table takes exactly its header and its rows, so the caption lands
    // against them rather than at the foot of the frame. When the table is
    // taller than the pane it scrolls and the caption takes the last line.
    let content = 1 + table.rows().len().min(max_rows) as u16;
    let table_height = if caption.is_some() && content < inner.height {
        content
    } else {
        inner.height.saturating_sub(u16::from(caption.is_some()))
    };
    let rows = Layout::vertical([
        Constraint::Length(table_height),
        Constraint::Length(u16::from(caption.is_some())),
    ])
    .split(inner);
    frame.render_stateful_widget(table_widget(table, inner.width, max_rows), rows[0], state);
    if let Some(text) = caption {
        frame.render_widget(
            Paragraph::new(text).style(crate::tui::theme::status()),
            rows[1],
        );
    }
    frame.render_widget(block, area);
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
        .title(Span::styled(format!(" {title} "), crate::tui::theme::tab()))
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
    let focus = app.focus;
    match focus {
        Focus::Databases => databases_tab(frame, rows[1], app),
        Focus::Service => service_tab(frame, rows[1], app),
        Focus::Backups => backups_tab(frame, rows[1], app),
        Focus::Keys => keys_tab(frame, rows[1], app),
        Focus::Queries => queries_tab(frame, rows[1], app),
        Focus::Locations => locations_tab(frame, rows[1], app),
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
///
/// The strip has to fit on one line, and a narrow terminal may not have room
/// for all of it. When it does not, the COUNTS go and the names stay: the
/// counts are the summary -- how much is in a tab -- and the names are the
/// promise that every tab exists. A tab that cannot be seen is a tab that
/// cannot be opened, so the sixth tab is never the one that gets cut off.
fn tabs(frame: &mut Frame, area: Rect, app: &App) -> Vec<(Focus, u16, u16)> {
    // The one list of tabs. The strip, the number keys and the command
    // bar's range are all built from it, so they cannot drift apart --
    // and they did drift, when the bar said "1-4" with five tabs on the
    // screen.
    let all = Focus::ALL;
    // What each tab holds, so the strip is a summary and not just a
    // switchboard: the count is the same number the tab's own heading
    // shows.
    let counts: Vec<Option<usize>> = all
        .iter()
        .map(|focus| match focus {
            Focus::Databases => app.databases().map(|table| table.rows().len()),
            Focus::Keys => app.keys_table().map(|table| table.rows().len()),
            Focus::Queries => app.query_table().map(|table| table.rows().len()),
            Focus::Locations => app.locations().map(|table| table.rows().len()),
            Focus::Service | Focus::Backups => None,
        })
        .collect();
    let with_counts = strip_labels(&all, &counts, true);
    let names_only = strip_labels(&all, &counts, false);
    let labels = if strip_width(&with_counts, app.focus) <= area.width as usize {
        with_counts
    } else if strip_width(&names_only, app.focus) <= area.width as usize {
        names_only
    } else {
        // Even the names alone do not fit: there is no honest way to draw
        // six tabs on one short line, so the line is clipped and the
        // number keys still reach the tabs that are not named.
        with_counts
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cells: Vec<(Focus, u16, u16)> = Vec::new();
    let mut used: u16 = 0;
    for (index, (focus, label)) in labels.iter().enumerate() {
        let active = *focus == app.focus;
        let style = if active {
            crate::tui::theme::tab_active()
        } else {
            crate::tui::theme::tab()
        };
        // Underlined means active, so the marker survives a monochrome
        // terminal; the caret is a second, plainer signal.
        let text = if active {
            format!("▸{label}")
        } else {
            label.clone()
        };
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
        if index + 1 < labels.len() {
            spans.push(Span::styled("│", crate::tui::theme::border()));
            used = used.saturating_add(1);
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    cells
}

/// The strip's labels, with their counts or without them.
fn strip_labels(
    all: &[Focus],
    counts: &[Option<usize>],
    with_counts: bool,
) -> Vec<(Focus, String)> {
    all.iter()
        .zip(counts)
        .enumerate()
        .map(|(index, (focus, count))| {
            let count = count.filter(|_| with_counts);
            let label = match count {
                Some(count) => format!(" {} {} ({count}) ", index + 1, focus.title()),
                None => format!(" {} {} ", index + 1, focus.title()),
            };
            (*focus, label)
        })
        .collect()
}

/// How wide a strip of labels is, with the separators and the active tab's
/// caret.
fn strip_width(labels: &[(Focus, String)], active: Focus) -> usize {
    let width: usize = labels
        .iter()
        .map(|(focus, label)| label.width() + usize::from(*focus == active))
        .sum();
    width + labels.len().saturating_sub(1)
}

/// The database list, with each database's traffic.
///
/// The tab says how many databases it holds: a list that silently
/// cuts itself is a visibility lie, and the count is what tells the
/// operator to scroll.
///
/// This tab answers ONE question -- what the databases have done -- and
/// it gives the whole body to it. Where the files are used to be a
/// second box under this one, which split the body in two and made both
/// answers smaller: the placement table took what the traffic table
/// left, and on a screen with enough databases the placement table was
/// squeezed to a header and a row. Placement is its own tab now, and
/// each tab answers one question.
fn databases_tab(frame: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus == Focus::Databases;
    let Some(traffic) = app.databases() else {
        let block = pane(focused);
        frame.render_widget(Paragraph::new("no answer yet").block(block), area);
        return;
    };
    // The scroll cannot run past the last row: a tab that scrolls into
    // nothing is a tab that says "there is nothing here".
    app.clamp_scroll(
        traffic.rows().len(),
        app.visible_table_rows(traffic.footer_text().is_some()),
    );
    let mut state = TableState::default().with_offset(app.scroll.databases as usize);
    framed_table(frame, area, pane(focused), &traffic, usize::MAX, &mut state);
}

/// Where each database lives on disk, and whether it is there.
///
/// "Is everything live and working" needs the file, not only the
/// counters: a live registry row whose file is gone does no work at all,
/// and the counters would say nothing about that. So the placement has
/// its own tab, where it is the only answer and gets the whole body.
///
/// A gateway that does not report its placement says so in words. An
/// empty table would read as "there is nowhere they are", which is a
/// different and much worse claim. A status answer that has not
/// arrived is a different thing again, and is named as such: blaming
/// the gateway for a question nobody has answered yet is its own kind
/// of wrong.
fn locations_tab(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = pane(app.focus == Focus::Locations);
    let Some(locations) = app.locations() else {
        let note = if app.status.is_none() {
            "no answer yet"
        } else {
            "this gateway does not report where its databases live"
        };
        frame.render_widget(Paragraph::new(note).block(block), area);
        return;
    };
    app.clamp_scroll(
        locations.rows().len(),
        app.visible_table_rows(locations.footer_text().is_some()),
    );
    let mut state = TableState::default().with_offset(app.scroll.locations as usize);
    framed_table(
        frame,
        area,
        pane_titled("locations"),
        &locations,
        usize::MAX,
        &mut state,
    );
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
fn service_tab(frame: &mut Frame, area: Rect, app: &mut App) {
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
    // The scroll cannot run past the last line: a dashboard scrolled into
    // nothing reads as a gateway with nothing to say.
    app.clamp_scroll(lines.len(), app.visible_rows());
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
///
/// The shared view builds two tables here — the published sets, and each
/// database's freshness — and the view used to draw only the first of them,
/// because it kept "the table" rather than "the tables". The freshness table
/// is the one this tab exists for, and `docs/tui.md` promises it. The test
/// that should have caught that looked for the substring "fresh", which
/// every screen contains in the word "refresh" on the command bar.
fn backups_tab(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = pane(app.focus == Focus::Backups);
    let Some(backups) = &app.backups else {
        frame.render_widget(Paragraph::new("no answer yet").block(block), area);
        return;
    };
    let sections = views::backups(backups);
    let mut lines = Vec::new();
    let mut tables: Vec<(String, ViewTable)> = Vec::new();
    for section in &sections {
        // The same rule as the service tab: a heading is only drawn when
        // there is something under it.
        if section_has_lines(section) {
            lines.push(heading(section.title()));
            lines.extend(section_lines(section));
        }
        if let Some(found) = section_table(section) {
            // The table's title comes from the section that owns it. It used
            // to be hardcoded to "per-database", so the *published sets*
            // table was labelled per-database -- a title that names the wrong
            // thing is worse than no title.
            tables.push((section.title().to_string(), found.clone()));
        }
    }
    // The fields answer the questions that have no other home: where the
    // off-host copy is, when it last ran, whether it was ever restored.
    // The tables are lists, and a list can scroll -- but the per-database
    // freshness table is not a list of that kind. It is this tab's
    // answer, one row per database, and docs/tui.md promises that it
    // keeps its rows whatever else is on screen.
    //
    // That promise was broken by the "snapshot cadence" field. The
    // fields took their room first, so on a short terminal the freshness
    // table lost a row: a database could be named as missing in the
    // coverage block while its state was clipped off the bottom, and the
    // operator was told a fact about a database they could not see.
    //
    // So the fields keep their room first and the table takes what is
    // left: with a row per database, thirty databases want more room
    // than any terminal has, and making that whole would push the facts
    // about the copy off the screen, which is the failure the 0.7.0
    // release fixed and its test still protects. The repair below is the
    // other half: it fires only for a small table, where making it whole
    // costs a few scrolled field lines and removes the contradiction.
    //
    // A table that has nothing but its header is a table with no answer
    // in it, so each table is promised room for a header and one row
    // before the fields are given anything.
    let min_table = 4u16;
    // The floor the fields never drop below: the root facts and the
    // first headings stay on screen even on a tiny terminal.
    let fields_floor = 6u16;
    // Every row the freshness table has, plus the frame around it.
    let per_database_full = tables
        .first()
        .map(|(_, table)| table.rows().len() as u16 + 3)
        .unwrap_or(0);
    // The tables other than the freshness one get their minimum promise.
    let other_tables_min = min_table * tables.len().saturating_sub(1) as u16;
    // The room the fields need to show what they carry.
    let fields_wanted = lines.len() as u16 + 2;
    // The fields are the tab's entry point: they keep their room first
    // and the freshness table takes what is left, scrolling when there
    // is more of it than fits. That is what keeps a thirty-database
    // deployment readable -- the facts about the copy stay on screen
    // and the long list scrolls.
    let mut fields_height = fields_wanted.min(
        area.height
            .saturating_sub(other_tables_min)
            .saturating_sub(min_table),
    );
    let mut per_database_height = if tables.is_empty() {
        0
    } else {
        per_database_full.min(
            area.height
                .saturating_sub(fields_height)
                .saturating_sub(other_tables_min),
        )
    };
    // ...but a small table clipped while the summary names its missing
    // row is the exact contradiction this tab exists to avoid: the
    // coverage block says "missing: other" and the row that says whether
    // that is stale or fresh is cut off the bottom. So when the table is
    // partial, a database the coverage block names is clipped out of it,
    // and making the table whole still leaves the fields a readable
    // window, the table is made whole and the fields scroll instead.
    // The repair is deliberately capped at a small table: a table with
    // more rows than half the tab is a list, and lists scroll -- making
    // a thirty-row table whole would push the facts about the copy off
    // the screen, which is the failure the 0.7.0 release fixed.
    if !tables.is_empty() && per_database_height < per_database_full {
        let visible_rows = per_database_height.saturating_sub(3) as usize;
        let names: Vec<&str> = backups["per_database"]
            .as_object()
            .map(|map| {
                let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
                keys.sort_unstable();
                keys
            })
            .unwrap_or_default();
        let missing: Vec<&str> = backups["coverage"]["missing"]
            .as_array()
            .map(|list| list.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let names_beyond: Vec<&str> = names.iter().skip(visible_rows).copied().collect();
        let contradiction = missing.iter().any(|name| names_beyond.contains(name));
        let small_enough = per_database_full <= area.height / 2;
        let repaired_fields = area
            .height
            .saturating_sub(per_database_full)
            .saturating_sub(other_tables_min);
        if contradiction && small_enough && repaired_fields >= fields_floor {
            per_database_height = per_database_full;
            fields_height = repaired_fields.min(fields_wanted);
        }
    }
    let mut constraints = vec![Constraint::Length(fields_height)];
    if !tables.is_empty() {
        constraints.push(Constraint::Length(per_database_height));
    }
    for _ in tables.iter().skip(1) {
        constraints.push(Constraint::Min(min_table));
    }
    let rows = Layout::vertical(constraints).split(area);
    let rows_pane_height = rows
        .get(rows.len().saturating_sub(1))
        .map(|rect| rect.height)
        .unwrap_or(0);
    // Scrolling moves whatever is cut off. The fields are the last thing
    // to be cut off -- they hold the facts that have no other home -- and
    // of the two tables the per-database one is cut off first, because it
    // grows with the deployment while the published-sets log does not.
    // A section below the fold is still reachable.
    let fields_clipped = fields_height < lines.len() as u16 + 2;
    let per_database_clipped = tables
        .first()
        .map(|(_, table)| per_database_height < table.rows().len() as u16 + 3)
        .unwrap_or(false);
    // The scroll cannot run past the end of whatever it is scrolling. A
    // tab scrolled into nothing reads as a service with nothing to say.
    let (content, visible) = if fields_clipped {
        (lines.len(), (fields_height as usize).saturating_sub(2))
    } else if per_database_clipped {
        let table_rows = tables
            .first()
            .map(|(_, table)| table.rows().len())
            .unwrap_or(0);
        (
            table_rows,
            (per_database_height as usize).saturating_sub(3).max(1),
        )
    } else {
        let table_rows = tables
            .last()
            .map(|(_, table)| table.rows().len())
            .unwrap_or(0);
        let caption = tables
            .last()
            .map(|(_, table)| table.footer_text().is_some())
            .unwrap_or(false);
        // The flexible table has a pane of its own, and its height was
        // just measured by the layout.
        (
            table_rows,
            (rows_pane_height as usize)
                .saturating_sub(3 + usize::from(caption))
                .max(1),
        )
    };
    app.clamp_scroll(content, visible);
    let offset = app.scroll.backups as usize;
    let lines: Vec<Line<'static>> = if fields_clipped {
        lines.into_iter().skip(offset).collect()
    } else {
        lines
    };
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), rows[0]);
    let last = tables.len().saturating_sub(1);
    for (index, (title, table)) in tables.iter().enumerate() {
        // Only the cut-off table scrolls: a table that keeps its rows
        // must not lose its first row to a scroll meant for another one.
        let mut state = TableState::default();
        let scrolling = match index {
            0 => per_database_clipped,
            _ => index == last && !fields_clipped && !per_database_clipped,
        };
        if scrolling {
            state = TableState::default().with_offset(offset);
        }
        framed_table(
            frame,
            rows[index + 1],
            pane_titled(title),
            table,
            usize::MAX,
            &mut state,
        );
    }
}

/// The credentials that exist.
fn keys_tab(frame: &mut Frame, area: Rect, app: &mut App) {
    match &app.keys {
        Some(keys) => {
            let table = views::keys(keys);
            let block = pane(app.focus == Focus::Keys);
            app.clamp_scroll(
                table.rows().len(),
                app.visible_table_rows(table.footer_text().is_some()),
            );
            let mut state = TableState::default().with_offset(app.scroll.keys as usize);
            framed_table(frame, area, block, &table, usize::MAX, &mut state);
        }
        None => {
            let block = pane(app.focus == Focus::Keys);
            frame.render_widget(Paragraph::new("no answer yet").block(block), area);
        }
    }
}

/// The statement shapes the service has run, and what each one cost.
///
/// The tab answers "which query is heavy", which is a question only
/// the service itself can answer: every statement passes through the
/// gateway on its way to SQLite. What is shown is the SHAPE of a
/// statement -- its SQL with every literal replaced by a placeholder --
/// so a row names a statement without quoting the data it carried.
fn queries_tab(frame: &mut Frame, area: Rect, app: &mut App) {
    match &app.queries {
        Some(payload) => {
            let table = views::query_table(payload);
            let block = pane(app.focus == Focus::Queries);
            app.clamp_scroll(
                table.rows().len(),
                app.visible_table_rows(table.footer_text().is_some()),
            );
            let mut state = TableState::default().with_offset(app.scroll.queries as usize);
            framed_table(frame, area, block, &table, usize::MAX, &mut state);
        }
        None => {
            let block = pane(app.focus == Focus::Queries);
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
        .map(|table| {
            // The caption sits under the rows, so a table with a footer needs
            // one more line than one without.
            let caption = u16::from(table.footer_text().is_some());
            (table.rows().len().min(OUTCOME_ROWS) as u16) + 3 + caption
        })
        .unwrap_or(0);
    let rows = Layout::vertical([
        Constraint::Length(facts + 2),
        Constraint::Length(table_lines),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(pane_titled("last action")),
        rows[0],
    );
    if let Some(table) = &outcome.table {
        framed_table(
            frame,
            rows[1],
            pane_titled("rows"),
            table,
            OUTCOME_ROWS,
            &mut TableState::default(),
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
    // The tab range is counted from the one list of tabs, so it cannot
    // fall behind the strip. It DID fall behind it: the bar said "1-4"
    // while a fifth tab was on the screen, because the range was written
    // down by hand and only the strip was updated.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used: usize = 0;
    let tabs = format!("1-{}", Focus::ALL.len());
    spans.push(Span::styled(tabs.clone(), crate::tui::theme::key()));
    used += tabs.width();
    spans.push(Span::styled(" tabs", crate::tui::theme::key_label()));
    used += " tabs".width();
    // Key, then what it does. Ordered by how often it is reached for.
    const HINTS: [(&str, &str); 5] = [
        ("tab", "next tab"),
        ("↑↓", "scroll"),
        (":", "commands"),
        ("r", "refresh"),
        ("q", "quit"),
    ];
    for (key, label) in HINTS.iter() {
        spans.push(Span::styled("  ", crate::tui::theme::status()));
        spans.push(Span::styled(*key, crate::tui::theme::key()));
        spans.push(Span::styled(
            format!(" {label}"),
            crate::tui::theme::key_label(),
        ));
        used += 2 + key.width() + label.width() + 1;
    }
    if let Some(error) = &app.error {
        spans.push(Span::styled("  ·  ", crate::tui::theme::status()));
        used += 5;
        // The error is the last thing on a one-line bar, so it is cut to
        // what is left -- and the cut says so. A message that runs off
        // the edge without saying it is a message half-read.
        let room = (area.width as usize).saturating_sub(used);
        let text = if error.width() > room {
            let cut: String = error.chars().take(room.saturating_sub(1)).collect();
            format!("{cut}…")
        } else {
            error.clone()
        };
        spans.push(Span::styled(text, paint_style(ViewStyle::Red)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The list of actions, over the dashboard.
///
/// The destructive ones are MARKED, because a palette that lists
/// "delete database" beside "refresh" with the same weight tells the
/// operator that the two are the same kind of thing, and they are not.
/// The marker is a character and not only a colour, so it survives a
/// monochrome terminal.
fn palette(frame: &mut Frame, area: Rect, app: &App) {
    let actions = app.filtered_actions();
    let height = (actions.len() as u16 + 4).clamp(5, area.height.saturating_sub(2));
    let width = area.width.saturating_sub(6).clamp(30, 100);
    let popup = centered(area, width, height);
    // The title column is MEASURED, not assumed. It used to be a fixed
    // sixteen characters, and "rotate owner key" is exactly sixteen, so
    // that row ran its hint straight into its title with no gap between
    // them. Measuring the widest title that is actually on screen and
    // adding a separator means a longer action added later cannot
    // reintroduce the collision.
    let title_width = actions
        .iter()
        .map(|action| action.title.chars().count())
        .max()
        .unwrap_or(0)
        + 1;
    let mut lines = Vec::new();
    for (index, action) in actions.iter().enumerate() {
        let selected = index == app.highlighted;
        let style = if selected {
            crate::tui::theme::selection()
        } else {
            crate::tui::theme::paint(ViewStyle::Plain)
        };
        // A destructive action wears the theme's colour for a destructive
        // act -- the colour that existed for this and had no use until
        // now -- and a marker in front of its title.
        let marked = if action.destructive {
            crate::tui::theme::destructive()
        } else {
            style
        };
        lines.push(Line::from(vec![
            Span::styled(if selected { "▸ " } else { "  " }, style),
            Span::styled(if action.destructive { "! " } else { "  " }, marked),
            Span::styled(
                format!("{:<width$}", action.title, width = title_width),
                marked,
            ),
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
