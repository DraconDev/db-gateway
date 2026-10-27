//! The terminal operator view.
//!
//! One screen of four tabs — databases, service, backups and keys — each
//! given the whole body of the screen rather than a tile, with the strip
//! always naming every tab so a closed one is still discoverable. The
//! operator acts on what the open tab shows.
//!
//! Three rules hold this module together:
//!
//! * **The facts come from the shared view layer** ([`crate::views`]),
//!   the same functions the CLI draws from, so the two surfaces cannot
//!   tell different stories about the same gateway.
//! * **The requests come from the shared action catalogue**
//!   ([`actions`]), so the view cannot invent a call the CLI would not
//!   make, and the destructive ones are confirmed by the same rule.
//! * **The terminal is always given back** ([`screen`]), including when
//!   the view panics.

pub mod actions;
pub mod draw;
pub mod screen;
pub mod theme;

use crate::{
    client,
    error::{Error, Result},
    render, transfer, views,
};
use actions::{Action, ActionKind, Invocation};
use draw::Geometry;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use screen::Screen;
use serde_json::Value;
use std::time::{Duration, Instant};

/// How often the databases' contents are read again.
///
/// Slower than the traffic poll on purpose. The traffic counters are
/// read from the gateway's memory; the contents are read by opening
/// every database file and counting every row. Counting is not work to
/// do every two seconds for a service nobody is touching, and the
/// figures move only when someone writes.
const CONTENTS_PERIOD: Duration = Duration::from_secs(30);

/// How the view was started.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// How often the service is read again, in seconds.
    pub refresh_secs: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self { refresh_secs: 2 }
    }
}

/// What the view is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The dashboard.
    Normal,
    /// The list of actions.
    Palette,
    /// The arguments of the chosen action.
    Prompt { kind: ActionKind },
    /// The phrase that confirms a destructive act.
    Confirm {
        invocation: Invocation,
        phrase: String,
    },
    /// The condition narrowing the rows browser.
    ///
    /// Its own mode rather than a palette prompt: the text is a SQL
    /// condition, it is edited rather than typed from scratch, and its
    /// Enter re-reads the SAME table instead of running an action.
    Filter,
}

/// Which tab has the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    /// The database list.
    Databases,
    /// The service dashboard.
    Service,
    /// The backup freshness.
    Backups,
    /// The credentials.
    Keys,
    /// The statement shapes the service has run.
    Queries,
    /// Where each database lives on disk, and whether it is there.
    Locations,
}

impl Focus {
    /// Every tab, in the order the strip shows them.
    ///
    /// This list is the ONLY place the tabs are written down. The strip,
    /// the number keys, the tab/shift-tab cycle and the command bar's
    /// range are all built from it, so they cannot disagree about how
    /// many tabs there are or which key opens which one.
    ///
    /// It used to be written down in six places, and they drifted: the
    /// command bar still said "1-4" after a fifth tab existed, because
    /// only one of the six lists had been updated. A list that is
    /// written once cannot drift.
    pub const ALL: [Focus; 6] = [
        Focus::Databases,
        Focus::Service,
        Focus::Backups,
        Focus::Keys,
        Focus::Queries,
        Focus::Locations,
    ];

    /// The tab's place in the strip, counted from zero.
    ///
    /// The position is the tab's NUMBER on screen, so the number a
    /// key opens and the place in the strip are the same fact.
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    /// The tab a number opens, counting from one.
    ///
    /// `None` for a number no tab answers, so a key that names no tab
    /// opens nothing rather than opening the wrong one.
    pub fn from_number(number: usize) -> Option<Self> {
        number
            .checked_sub(1)
            .and_then(|index| Self::ALL.get(index).copied())
    }

    /// The next tab.
    pub fn next(self) -> Self {
        let index = self.index();
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// The previous tab, so `shift-tab` goes back the way it came.
    pub fn previous(self) -> Self {
        let index = self.index();
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// What the tab is called.
    pub fn title(self) -> &'static str {
        match self {
            Self::Databases => "databases",
            Self::Service => "service",
            Self::Backups => "backups",
            Self::Keys => "keys",
            Self::Queries => "queries",
            Self::Locations => "locations",
        }
    }
}

/// What an action did.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// What happened, in one line.
    pub headline: String,
    /// The facts it produced, as a person reads them.
    pub facts: Vec<(String, String)>,
    /// The rows it produced, when it produced rows.
    pub table: Option<render::Table>,
    /// Why it failed, when it failed.
    pub error: Option<String>,
}

/// A table open in the rows browser.
///
/// The browser is a place the operator can stay in, not a one-shot read:
/// paging, sorting and filtering are all questions about the SAME table,
/// so the table, the page over it and the choices narrowing it live here
/// together. Losing them on the next keystroke would turn every key into
/// a fresh start.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Browser {
    /// The database the table is in.
    pub database: String,
    /// The table being read.
    pub table: String,
    /// Which page, counting from zero.
    pub page: u64,
    /// The column it is sorted by, and which way.
    pub sort: Option<(String, crate::views::SortDirection)>,
    /// The condition narrowing the rows, when there is one.
    pub filter: Option<String>,
    /// How many rows the current filter selects.
    ///
    /// The table's own total under the filter, not the page's length:
    /// `rows 11-20 of N` is a statement about the whole answer, and a
    /// footer that said 10 would make every page look like the last.
    pub total: u64,
}

impl Browser {
    /// A browser on the first page of a table, unsorted and unfiltered.
    pub fn new(database: &str, table: &str) -> Self {
        Self {
            database: database.to_string(),
            table: table.to_string(),
            page: 0,
            sort: None,
            filter: None,
            total: 0,
        }
    }

    /// The first row number on this page, counting from one, as an
    /// operator counts.
    pub fn first_row(&self) -> u64 {
        self.page
            .saturating_mul(crate::views::ROWS_PAGE)
            .saturating_add(1)
    }

    /// The last row number on this page, or zero when the page is past
    /// the end of the answer.
    pub fn last_row(&self) -> u64 {
        if self.total == 0 || self.first_row() > self.total {
            return 0;
        }
        (self.first_row() + crate::views::ROWS_PAGE - 1).min(self.total)
    }

    /// Whether there is a page after this one.
    pub fn has_next(&self) -> bool {
        self.last_row() != 0 && self.last_row() < self.total
    }

    /// Whether there is a page before this one.
    pub fn has_previous(&self) -> bool {
        self.page > 0
    }

    /// The footer that says which slice of the answer is on screen.
    pub fn page_footer(&self) -> String {
        let mut parts = Vec::new();
        if self.total == 0 {
            parts.push("no rows match".to_string());
        } else {
            parts.push(format!(
                "rows {}-{} of {}",
                self.first_row(),
                self.last_row(),
                self.total
            ));
        }
        if let Some((column, direction)) = &self.sort {
            let arrow = match direction {
                crate::views::SortDirection::Ascending => "asc",
                crate::views::SortDirection::Descending => "desc",
            };
            parts.push(format!("sorted by {column} {arrow}"));
        }
        if let Some(filter) = &self.filter {
            parts.push(format!("filter: {filter}"));
        }
        let mut keys = Vec::new();
        if self.has_previous() {
            keys.push("[p] prev");
        }
        if self.has_next() {
            keys.push("[n] next");
        }
        keys.push("[s] structure");
        parts.push(keys.join("  "));
        parts.join("   ")
    }
}

/// How far each tab is scrolled.
///
/// A tab that silently cuts its list is a visibility lie: the operator
/// cannot tell a database that does not exist from one that is below
/// the fold. So the tabs scroll, and they say how much they hold.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scroll {
    /// The database list.
    pub databases: u16,
    /// The service dashboard.
    pub service: u16,
    /// The backup freshness.
    pub backups: u16,
    /// The credentials.
    pub keys: u16,
    /// The statement shapes.
    pub queries: u16,
    /// Where the databases live on disk.
    pub locations: u16,
}

impl Scroll {
    /// The offset of the open tab.
    pub fn of(&mut self, focus: Focus) -> &mut u16 {
        match focus {
            Focus::Databases => &mut self.databases,
            Focus::Service => &mut self.service,
            Focus::Backups => &mut self.backups,
            Focus::Keys => &mut self.keys,
            Focus::Queries => &mut self.queries,
            Focus::Locations => &mut self.locations,
        }
    }

    /// Keep the scroll inside what the tab holds.
    ///
    /// A tab that scrolls past its last row shows nothing, and an empty
    /// tab reads as "there is nothing here" -- a different and much worse
    /// claim than "you have reached the end". The offset also ran away
    /// without this: ten presses down on a two-row table needed ten
    /// presses up to come back, though only one press had moved
    /// anything.
    pub fn clamp(&mut self, focus: Focus, max: usize) {
        let offset = self.of(focus);
        *offset = (*offset as usize).min(max) as u16;
    }
}

/// The view's state.
pub struct App {
    /// The gateway being watched.
    pub server: String,
    /// The credential the view uses. Never drawn.
    key: String,
    /// The gateway's answer about the service.
    pub status: Option<Value>,
    /// The gateway's answer about the backups.
    pub backups: Option<Value>,
    /// The gateway's answer about the credentials.
    pub keys: Option<Value>,
    /// The gateway's answer about the databases.
    pub list: Option<Value>,
    /// The gateway's answer about the statement shapes it has run.
    pub queries: Option<Value>,
    /// How much data each database holds: tables and rows.
    ///
    /// Read on its own, slower cadence than the traffic poll: counting
    /// every row of every database means opening every file, which is
    /// not work to do every two seconds for an idle service.
    pub contents: Option<Value>,
    /// When the contents were last read, so the cadence can be its own.
    pub contents_refreshed: Option<Instant>,
    /// When the answers last arrived.
    pub refreshed: Option<Instant>,
    /// Whether the gateway answered the last refresh.
    pub connected: bool,
    /// The last thing that went wrong.
    pub error: Option<String>,
    /// What the view is doing.
    pub mode: Mode,
    /// The palette's filter.
    pub filter: String,
    /// The palette's highlighted entry.
    pub highlighted: usize,
    /// The line the operator is typing.
    pub input: String,
    /// Arguments already typed for the next prompt.
    ///
    /// The databases tab's Enter doorway fills this with the database
    /// under the scroll, so choosing the action drops the operator
    /// into its prompt with the database already typed. Typing in the
    /// palette clears it: a filter the operator changed no longer
    /// promises the action the doorway meant.
    pub prompt_prefill: String,
    /// The last action's outcome.
    pub outcome: Option<Outcome>,
    /// The rows browser, when a table is open in it.
    ///
    /// It holds what the operator is looking at -- which table, which
    /// page, how it is sorted, what it is filtered by -- so the next
    /// keystroke can ask the gateway for the next page of the same
    /// question rather than starting over.
    pub browser: Option<Browser>,
    /// Which tab has the keyboard.
    pub focus: Focus,
    /// How far each tab is scrolled.
    pub scroll: Scroll,
    /// Where the last frame put the tabs and the body, so a click can be
    /// resolved against what was actually drawn rather than a second guess
    /// at the layout.
    pub geometry: Geometry,
    /// Where the pointer is, in cells, as the last mouse event said.
    ///
    /// The view marks the row under the pointer, so the operator can see
    /// which row a click would take before taking it. A pointer the
    /// terminal has never reported is not a pointer, and a row that is
    /// marked without a pointer under it would be a lie.
    pub mouse: Option<(u16, u16)>,
    /// The action waiting to run.
    pending: Option<Invocation>,
    /// Whether the view should end.
    pub quit: bool,
}

impl App {
    /// A view of one gateway.
    pub fn new(server: String, key: String) -> Self {
        Self {
            server,
            key,
            status: None,
            backups: None,
            keys: None,
            list: None,
            queries: None,
            contents: None,
            contents_refreshed: None,
            refreshed: None,
            connected: false,
            error: None,
            mode: Mode::Normal,
            filter: String::new(),
            highlighted: 0,
            input: String::new(),
            prompt_prefill: String::new(),
            outcome: None,
            browser: None,
            focus: Focus::Databases,
            scroll: Scroll::default(),
            geometry: Geometry::default(),
            mouse: None,
            pending: None,
            quit: false,
        }
    }

    /// The actions the palette shows, filtered by what was typed.
    pub fn filtered_actions(&self) -> Vec<&'static Action> {
        let needle = self.filter.to_lowercase();
        actions::CATALOG
            .iter()
            .filter(|action| {
                needle.is_empty()
                    || action.title.to_lowercase().contains(&needle)
                    || action.hint.to_lowercase().contains(&needle)
            })
            .collect()
    }

    /// The action waiting to run, taken.
    pub fn take_pending(&mut self) -> Option<Invocation> {
        self.pending.take()
    }

    /// Handle one key.
    pub fn on_key(&mut self, key: KeyEvent) {
        if key
            .modifiers
            .contains(ratatui::crossterm::event::KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('c')
        {
            // The way out of a program that has taken the terminal.
            self.quit = true;
            return;
        }
        match self.mode.clone() {
            Mode::Normal => self.on_key_normal(key),
            Mode::Palette => self.on_key_palette(key),
            Mode::Prompt { .. } | Mode::Confirm { .. } => self.on_key_input(key),
            Mode::Filter => self.on_key_filter(key),
        }
    }

    /// Handle one mouse event.
    ///
    /// Only the gestures the view can honour are acted on, and each maps to
    /// a keystroke the operator already has: a click on the strip's label
    /// goes to that tab, the wheel scrolls the open tab, and a click in the
    /// body focuses it so the keyboard follows the mouse. Nothing here can
    /// reach a destructive action -- those still need their typed
    /// confirmation -- so a stray click cannot change the gateway.
    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        // A click while a prompt or the palette is open belongs to that
        // dialogue; the view underneath is not the thing on top.
        if self.mode != Mode::Normal {
            return;
        }
        match mouse.kind {
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                // The pointer's place is remembered so the row under it
                // can be marked, and so a click without a move first
                // still knows which row it landed on.
                self.mouse = Some((mouse.column, mouse.row));
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.mouse = Some((mouse.column, mouse.row));
                self.on_click(mouse.column, mouse.row);
            }
            // Three lines a notch is what a terminal wheel usually carries,
            // and scrolling faster than the keyboard's single line is what
            // makes a long table usable with a trackpad.
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::ScrollDown => self.scroll_by(3),
            _ => {}
        }
    }

    /// Act on a left click at one cell.
    fn on_click(&mut self, column: u16, row: u16) {
        // The strip first: a label is a tab switch, which is the one thing a
        // click on a tab bar is expected to do.
        if self.geometry.strip_y == Some(row) {
            let hit = self
                .geometry
                .tabs
                .iter()
                .find(|(_, start, width)| column >= *start && column < start.saturating_add(*width))
                .map(|(focus, _, _)| *focus);
            if let Some(focus) = hit {
                self.focus = focus;
            }
            return;
        }
        // Then a row that names a database: clicking a database OPENS
        // it. The keyboard's doorway to a table's rows is Enter; the
        // mouse's doorway to the database itself is the click.
        if let Some(name) = self.database_under_pointer(column, row) {
            self.open_database(&name);
            return;
        }
        // Then a row of the last action's grid: the database's own
        // contents, one row per table. Clicking a table there opens
        // that table's structure -- the columns, their types and their
        // flags -- which is what "opening a table" means here.
        if let Some((database, table)) = self.table_under_pointer(column, row) {
            // Opening a table means opening its ROWS: the data is what
            // "open" means. Its structure is one key away on the browser
            // (`s`), so neither answer is hidden behind the other.
            self.pending = Some(Invocation::OpenTable {
                name: database,
                table,
                page: 0,
            });
            return;
        }
        // Then the body: clicking it takes the keyboard there, so the wheel
        // and the arrow keys act on the tab the operator clicked into.
        if let Some(body) = self.geometry.body
            && column >= body.x
            && column < body.x.saturating_add(body.width)
            && row >= body.y
            && row < body.y.saturating_add(body.height)
        {
            let offset = self.scroll.of(self.focus);
            if *offset > 0 {
                // Clicking a body that is scrolled away puts it back at the
                // top: a click is a request to read from the beginning.
                *offset = 0;
            }
        }
    }

    /// The database a click landed on, if it landed on one.
    ///
    /// The name is read from the row the operator clicked, in the table
    /// the tab drew, so the answer is the name that was on screen. A
    /// click on a table that does not name databases -- the keys, the
    /// queries -- opens nothing: those rows are not databases, and the
    /// view never reaches a destructive action from a click.
    fn database_under_pointer(&self, column: u16, row: u16) -> Option<String> {
        let table = match self.focus {
            Focus::Databases => self.databases()?,
            Focus::Locations => self.locations()?,
            _ => return None,
        };
        // The tab's own table is the first one drawn, and it is the only
        // one whose rows name databases. A table below it -- the last
        // action's rows -- is not a database list, and a click there
        // must not open whatever database happens to share its index.
        let drawn = self.geometry.tables.first()?;
        let index = drawn.row_at(column, row)?;
        let name = table.rows().get(index)?.first()?.parts().0.into_owned();
        // A row that names nothing usable is not a database.
        (!name.is_empty() && crate::store::validate_name(&name).is_ok()).then_some(name)
    }

    /// Open a database: what a click on its row does.
    ///
    /// Opening a database means reading its contents -- the tables it
    /// holds, with their row and column counts -- and the last action's
    /// area is where the answer appears. The gateway is asked for the
    /// structure, which is the one read that says what a database is
    /// made of, and the grid that comes back is the one a click on a
    /// table then opens further.
    fn open_database(&mut self, name: &str) {
        self.pending = Some(Invocation::Schema {
            name: name.to_string(),
            table: None,
        });
    }

    /// The table a click landed on in the last action's grid.
    ///
    /// Only the contents grid counts: its first column holds the table
    /// names, and its rows are the tables of the database that was
    /// opened. A structure grid -- a table's own columns -- has nothing
    /// to open, and the answer is None for it.
    fn table_under_pointer(&self, column: u16, row: u16) -> Option<(String, String)> {
        let drawn = self.geometry.outcome.as_ref()?;
        let index = drawn.row_at(column, row)?;
        let outcome = self.outcome.as_ref()?;
        let table = outcome.table.as_ref()?;
        // The contents grid is the one whose first column names tables.
        let first_column = table.columns().first()?.0.as_str();
        if first_column != "table" {
            return None;
        }
        let name = table.rows().get(index)?.first()?.parts().0.into_owned();
        let database = outcome
            .facts
            .iter()
            .find(|(key, _)| key == "database")
            .map(|(_, value)| value.clone())?;
        (crate::store::validate_name(&name).is_ok()).then_some((database, name))
    }

    fn on_key_normal(&mut self, key: KeyEvent) {
        // The rows browser owns the keyboard while it is open: its keys
        // are about the table on screen, and they would collide with the
        // dashboard's (`r` refreshes the dashboard, but here it means
        // "re-read these rows"). Esc leaves the browser first, so a
        // keystroke meant for the table cannot quit the whole view.
        if self.browser.is_some() && self.browser_key(&key) {
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('r') => self.pending = Some(Invocation::Refresh),
            KeyCode::Char(':') => {
                self.mode = Mode::Palette;
                self.filter.clear();
                self.highlighted = 0;
            }
            // The row grid is one keystroke from the databases tab:
            // Enter opens the palette on the open-table action, with
            // the database under the scroll already typed into the
            // action's prompt. The filter names the action so the
            // palette lists it first and selectable; choosing it
            // drops into the prompt with the database prefilled, so
            // the operator types only the table (and optionally the
            // page). Typing anything else clears the prefill first.
            //
            // The keyboard's doorway to a table's ROWS; a click is the
            // mouse's doorway to the database itself.
            KeyCode::Enter => {
                if self.focus == Focus::Databases {
                    let names = self.database_names();
                    let index = (self.scroll.databases as usize).min(names.len().saturating_sub(1));
                    self.prompt_prefill = match names.get(index) {
                        Some(name) => format!("{name} "),
                        None => String::new(),
                    };
                    self.filter = "open table".to_string();
                    self.mode = Mode::Palette;
                    self.highlighted = 0;
                }
            }
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => self.focus = self.focus.previous(),
            // The tab strip prints these numbers, so they have to work.
            // A numbered hint that does not do anything is worse than no
            // hint: it teaches the operator a key that fails. The range
            // and the tab it opens are both read from the one list of
            // tabs, so a tab that exists is always reachable by its
            // number.
            KeyCode::Char(digit @ '1'..='9') => {
                let number = digit as usize - '1' as usize + 1;
                if let Some(focus) = Focus::from_number(number) {
                    self.focus = focus;
                }
            }
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::PageDown => self.scroll_by(10),
            KeyCode::Home => *self.scroll.of(self.focus) = 0,
            _ => {}
        }
    }

    /// Scroll the open tab.
    fn scroll_by(&mut self, lines: i32) {
        let offset = self.scroll.of(self.focus);
        let moved = (*offset as i32 + lines).max(0) as u16;
        *offset = moved;
    }

    /// The rows browser's own keys. Returns whether the key belonged to
    /// the browser.
    ///
    /// Every one of these changes the QUESTION the browser is asking --
    /// which page, which order, what narrows it -- so each queues a
    /// re-read through one path. The table and the database never
    /// change here: `s` is how the operator leaves for the structure,
    /// and Esc how they leave entirely.
    fn browser_key(&mut self, key: &KeyEvent) -> bool {
        let Some(browser) = self.browser.clone() else {
            return false;
        };
        match key.code {
            KeyCode::Esc => {
                self.browser = None;
                self.outcome = None;
            }
            KeyCode::Char('q') => {
                // A browser is a place; leaving it is Esc. `q` still
                // quits the view, which is what it means everywhere.
                self.quit = true;
            }
            KeyCode::Char('n') => {
                if !browser.has_next() {
                    return true;
                }
                self.browser = Some(Browser {
                    page: browser.page + 1,
                    ..browser
                });
                self.pending = Some(Invocation::RefreshRows);
            }
            KeyCode::Char('p') => {
                if !browser.has_previous() {
                    return true;
                }
                self.browser = Some(Browser {
                    page: browser.page - 1,
                    ..browser
                });
                self.pending = Some(Invocation::RefreshRows);
            }
            // The structure of the table on screen: the door the browser
            // keeps beside it, rather than making the operator go back
            // out to the palette and type the name again.
            KeyCode::Char('s') => {
                self.pending = Some(Invocation::Schema {
                    name: browser.database.clone(),
                    table: Some(browser.table.clone()),
                });
                self.browser = None;
            }
            // A sort cycles: unordered, ascending, descending, unordered
            // again -- so one key both sorts and un-sorts.
            KeyCode::Char('o') => {
                let columns: Vec<String> = self
                    .outcome
                    .as_ref()
                    .and_then(|outcome| outcome.table.as_ref())
                    .map(|table| {
                        table
                            .columns()
                            .iter()
                            .map(|(name, _)| name.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                let next = match &browser.sort {
                    None => columns
                        .first()
                        .map(|column| (column.clone(), crate::views::SortDirection::Ascending)),
                    Some((column, crate::views::SortDirection::Ascending)) => Some((
                        column.clone(),
                        crate::views::SortDirection::Descending,
                    )),
                    Some(_) => None,
                };
                self.browser = Some(Browser { sort: next, ..browser });
                self.pending = Some(Invocation::RefreshRows);
            }
            KeyCode::Char('f') => {
                self.mode = Mode::Filter;
                // The condition is EDITED, not retyped: it opens with
                // what is already narrowing the rows.
                self.input = browser.filter.clone().unwrap_or_default();
            }
            KeyCode::Char('c') => {
                self.browser = Some(Browser {
                    filter: None,
                    page: 0,
                    ..browser
                });
                self.pending = Some(Invocation::RefreshRows);
            }
            _ => return false,
        }
        true
    }

    fn on_key_palette(&mut self, key: KeyEvent) {
        let count = self.filtered_actions().len();
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.filter.clear();
                self.prompt_prefill.clear();
            }
            KeyCode::Up => self.highlighted = self.highlighted.saturating_sub(1),
            KeyCode::Down => {
                if count > 0 {
                    self.highlighted = (self.highlighted + 1).min(count - 1);
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.highlighted = 0;
                self.prompt_prefill.clear();
            }
            KeyCode::Char(character) => {
                self.filter.push(character);
                self.highlighted = 0;
                self.prompt_prefill.clear();
            }
            KeyCode::Enter => self.choose(),
            _ => {}
        }
    }

    /// The keys of the filter prompt.
    ///
    /// Enter applies the condition to the table already on screen and
    /// re-reads it; an empty condition clears the filter, which is how
    /// the operator gets the whole table back. Esc abandons the edit and
    /// leaves the rows as they were.
    fn on_key_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(character) => self.input.push(character),
            KeyCode::Enter => {
                let Some(browser) = self.browser.clone() else {
                    self.mode = Mode::Normal;
                    self.input.clear();
                    return;
                };
                let typed = self.input.trim().to_string();
                self.mode = Mode::Normal;
                self.input.clear();
                self.browser = Some(Browser {
                    filter: (!typed.is_empty()).then_some(typed),
                    // A new condition selects a different set, so the
                    // page it was on means nothing: back to the first.
                    page: 0,
                    ..browser
                });
                self.pending = Some(Invocation::RefreshRows);
            }
            _ => {}
        }
    }

    fn on_key_input(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(character) => self.input.push(character),
            KeyCode::Enter => self.submit(),
            _ => {}
        }
    }

    /// The operator chose an action from the palette.
    fn choose(&mut self) {
        let Some(action) = self.filtered_actions().get(self.highlighted).copied() else {
            return;
        };
        if action.arguments {
            self.mode = Mode::Prompt { kind: action.kind };
            self.input = std::mem::take(&mut self.prompt_prefill);
            self.filter.clear();
            return;
        }
        if action.confirmation {
            self.prompt_prefill.clear();
            // An action whose confirmation is its whole input: the
            // owner key has no arguments, only a word.
            let Ok(invocation) = action.kind.parse("") else {
                return;
            };
            let Some(phrase) = invocation.confirmation() else {
                return;
            };
            self.mode = Mode::Confirm { invocation, phrase };
            self.input.clear();
            return;
        }
        self.mode = Mode::Normal;
        self.filter.clear();
        self.prompt_prefill.clear();
        match action.kind {
            ActionKind::Quit => self.quit = true,
            ActionKind::Refresh => self.pending = Some(Invocation::Refresh),
            _ => {}
        }
    }

    /// The operator answered a prompt.
    fn submit(&mut self) {
        let Mode::Prompt { kind } = self.mode.clone() else {
            // A confirmation: the phrase has to be exact. The rule is
            // the shared one, the same rule the CLI's `--confirm` flag
            // enforces.
            let Mode::Confirm { invocation, phrase } = self.mode.clone() else {
                return;
            };
            if crate::confirmation::confirm_matches(Some(&self.input), &phrase).is_err() {
                // The line is cleared, so the operator types the name
                // again rather than editing a wrong one in place.
                self.input.clear();
                self.outcome = Some(Outcome {
                    headline: "not confirmed".to_string(),
                    facts: Vec::new(),
                    table: None,
                    error: Some(format!("type {phrase} exactly to go ahead")),
                });
                return;
            }
            self.mode = Mode::Normal;
            self.input.clear();
            self.pending = Some(invocation);
            return;
        };
        let arguments = std::mem::take(&mut self.input);
        match kind.parse(&arguments) {
            Ok(invocation) => {
                if let Some(phrase) = invocation.confirmation() {
                    self.mode = Mode::Confirm { invocation, phrase };
                    self.input.clear();
                    return;
                }
                self.mode = Mode::Normal;
                self.pending = Some(invocation);
            }
            Err(error) => {
                self.mode = Mode::Normal;
                self.outcome = Some(Outcome {
                    headline: "the action was not understood".to_string(),
                    facts: Vec::new(),
                    table: None,
                    error: Some(error.message.to_string()),
                });
            }
        }
    }

    /// Read the service again.
    pub async fn refresh(&mut self) {
        // The five reads are independent, so they are made together rather
        // than one after another. In turn, a gateway that hung cost the
        // operator five read budgets -- fifty seconds of a frozen view,
        // because the loop was waiting on each read in turn and drew
        // nothing until the last one came back.
        let (status, backups, keys, list, queries) = tokio::join!(
            self.fetch("/v1/status"),
            self.fetch("/v1/backups"),
            self.fetch("/v1/keys"),
            self.fetch("/v1/databases"),
            self.fetch("/v1/queries"),
        );
        let mut failures: Vec<String> = Vec::new();
        // How many of the reads were answered. The threshold for "the
        // gateway is reachable" is counted rather than written down: it
        // WAS written down, as "fewer than four failures", which was
        // right when there were four reads and wrong the moment a fifth
        // one was added. The view then said "unreachable" while the
        // gateway was answering.
        let mut answered = 0usize;
        match status {
            Ok(value) => {
                self.status = Some(value);
                answered += 1;
            }
            Err(error) => failures.push(error.message.to_string()),
        }
        match backups {
            Ok(value) => {
                self.backups = Some(value);
                answered += 1;
            }
            Err(error) => failures.push(error.message.to_string()),
        }
        match keys {
            Ok(value) => {
                self.keys = Some(value);
                answered += 1;
            }
            Err(error) => failures.push(error.message.to_string()),
        }
        match list {
            Ok(value) => {
                self.list = Some(value);
                answered += 1;
            }
            Err(error) => failures.push(error.message.to_string()),
        }
        match queries {
            Ok(value) => {
                self.queries = Some(value);
                answered += 1;
            }
            Err(error) => failures.push(error.message.to_string()),
        }
        // The contents are read on their own, slower cadence: counting
        // every row of every database opens every file, and an idle
        // service should not pay for that every two seconds. A failure
        // here is NOT counted as a failed read of the dashboard: the
        // view is complete without it, and the contents columns say
        // "unknown" rather than the whole service reading as down.
        if self.contents_due() {
            if let Ok(value) = self.fetch("/v1/contents").await {
                self.contents = Some(value);
            }
            self.contents_refreshed = Some(Instant::now());
        }
        // The gateway is reachable when it answered anything at all: a
        // refused backup list does not mean the service is down.
        self.connected = answered > 0;
        // The bar has ONE line for the error, so the error says what
        // happened rather than listing every failure: five refusals of
        // the same kind are one fact, not five. The count of reads is
        // counted from the results, so a sixth read cannot leave the
        // message claiming there were five.
        let reads = answered + failures.len();
        self.error = if failures.is_empty() {
            None
        } else if failures.len() == 1 {
            Some(failures.remove(0))
        } else {
            Some(format!(
                "{} of {reads} reads failed: {}",
                failures.len(),
                failures[0]
            ))
        };
        self.refreshed = Some(Instant::now());
    }

    /// Read one page of a table into the browser.
    ///
    /// Three reads, because the browser needs three facts and the
    /// gateway answers each in its own words: the table's structure
    /// (which columns exist, so a sort names a real one), the count
    /// under the current filter (so the footer's `of N` is the answer's
    /// size and not the table's), and the page itself.
    ///
    /// The count is read BEFORE the page so the footer describes the
    /// page that is about to appear. If the count fails, the page is
    /// still shown and the footer says the total is unknown, rather than
    /// the whole read failing over a number.
    pub async fn open_browser(&mut self, mut browser: Browser) {
        let structure_path = format!("/v1/databases/{}/schema", browser.database);
        let structure = match self.fetch(&structure_path).await {
            Ok(value) => value,
            Err(error) => {
                self.outcome = Some(Outcome {
                    headline: "the table could not be opened".to_string(),
                    facts: vec![
                        ("database".to_string(), browser.database.clone()),
                        ("table".to_string(), browser.table.clone()),
                    ],
                    table: None,
                    error: Some(error.message.to_string()),
                });
                return;
            }
        };
        // The count, under the same filter the page will use. A refusal
        // here is not fatal: the page is still worth showing.
        let count = match crate::views::rows_count_query(
            &structure,
            &browser.table,
            browser.filter.as_deref(),
        ) {
            Ok(query) => self.query_sql(&browser.database, &query.sql).await.ok(),
            Err(refusal) => {
                self.outcome = Some(Outcome {
                    headline: "the table could not be opened".to_string(),
                    facts: Vec::new(),
                    table: None,
                    error: Some(refusal.message()),
                });
                return;
            }
        };
        if let Some(counted) = count.as_ref().and_then(count_total) {
            browser.total = counted;
        }
        let page = match crate::views::rows_query(
            &structure,
            &browser.table,
            browser.page,
            browser.sort.as_ref().map(|(column, direction)| {
                (column.as_str(), *direction)
            }),
            browser.filter.as_deref(),
        ) {
            Ok(query) => query,
            Err(refusal) => {
                self.outcome = Some(Outcome {
                    headline: "the table could not be opened".to_string(),
                    facts: Vec::new(),
                    table: None,
                    error: Some(refusal.message()),
                });
                return;
            }
        };
        match self.query_sql(&browser.database, &page.sql).await {
            Ok(response) => {
                self.outcome = Some(browser_outcome(&browser, &response));
                self.browser = Some(browser);
            }
            Err(error) => {
                self.outcome = Some(Outcome {
                    headline: "the table could not be read".to_string(),
                    facts: Vec::new(),
                    table: None,
                    error: Some(error.message.to_string()),
                });
            }
        }
    }

    /// Run one statement against one database, as a read.
    async fn query_sql(&self, database: &str, sql: &str) -> Result<Value> {
        let body = serde_json::json!({"sql": sql, "args": []});
        let value = client::request(
            &self.server,
            &self.key,
            reqwest::Method::POST,
            &format!("/v1/databases/{database}/query"),
            Some(body),
            None,
        )
        .await?;
        if let Some(error) = value.get("error") {
            let code = error["code"].as_str().unwrap_or("error");
            let message = error["message"].as_str().unwrap_or("refused");
            return Err(Error::with_message(
                400,
                "api_error",
                format!("{code}: {message}"),
            ));
        }
        Ok(value)
    }

    /// Read the page again with whatever the browser now says.
    ///
    /// Every control in the browser -- a page, a sort, a filter -- is a
    /// change to the same question, so each one re-reads through this
    /// one path rather than each assembling its own request.
    async fn reread_page(&mut self) {
        let Some(browser) = self.browser.clone() else {
            return;
        };
        self.open_browser(browser).await;
    }

    /// Whether the contents are due to be read again.
    ///
    /// They are on their own clock because they cost their own work:
    /// the traffic counters come from memory, and the contents come
    /// from opening every database file and counting its rows. A
    /// shorter period than this would make an idle view do disk work it
    /// has no reason to do.
    pub fn contents_due(&self) -> bool {
        match self.contents_refreshed {
            None => true,
            Some(at) => at.elapsed() >= CONTENTS_PERIOD,
        }
    }

    /// One read of the gateway.
    async fn fetch(&self, path: &str) -> Result<Value> {
        let value = client::request(
            &self.server,
            &self.key,
            reqwest::Method::GET,
            path,
            None,
            None,
        )
        .await?;
        if let Some(error) = value.get("error") {
            let code = error["code"].as_str().unwrap_or("error");
            let message = error["message"].as_str().unwrap_or("refused");
            return Err(Error::with_message(
                400,
                "api_error",
                format!("{code}: {message}"),
            ));
        }
        Ok(value)
    }

    /// Run the action the operator chose.
    pub async fn run_invocation(&mut self, invocation: Invocation) {
        // A table opened for its rows goes to the browser, which needs
        // several reads and keeps state between them. It is handled
        // before the one-request actions for that reason.
        if let Invocation::OpenTable { name, table, page } = &invocation {
            let browser = Browser {
                database: name.clone(),
                table: table.clone(),
                page: *page,
                ..Browser::new(name, table)
            };
            self.open_browser(browser).await;
            return;
        }
        let outcome = match &invocation {
            Invocation::Quit => {
                self.quit = true;
                return;
            }
            Invocation::Refresh => {
                self.refresh().await;
                return;
            }
            Invocation::RefreshRows => {
                self.reread_page().await;
                return;
            }
            Invocation::ExportDatabase { name, out } => {
                match transfer::export(&self.server, &self.key, name, out).await {
                    Ok(report) => Outcome {
                        headline: format!("exported {name}"),
                        facts: vec![
                            ("file".to_string(), report.out.display().to_string()),
                            ("size".to_string(), render::human_bytes(report.size)),
                            ("sha256".to_string(), report.sha256),
                        ],
                        table: None,
                        error: None,
                    },
                    Err(failure) => Outcome {
                        headline: "the export failed".to_string(),
                        facts: Vec::new(),
                        table: None,
                        error: Some(failure.error.message.to_string()),
                    },
                }
            }
            Invocation::ImportDatabase { name, from } => {
                match transfer::import(&self.server, &self.key, name, from).await {
                    Ok(report) => Outcome {
                        headline: format!("imported {name}"),
                        facts: vec![
                            ("file".to_string(), report.from.display().to_string()),
                            ("size".to_string(), render::human_bytes(report.size)),
                        ],
                        table: None,
                        error: None,
                    },
                    Err(failure) => Outcome {
                        headline: "the import failed".to_string(),
                        facts: Vec::new(),
                        table: None,
                        error: Some(failure.error.message.to_string()),
                    },
                }
            }
            _ => {
                let request = match invocation.request() {
                    Ok(request) => request,
                    Err(error) => {
                        // A refusal the client made: nothing was sent.
                        self.outcome = Some(Outcome {
                            headline: "nothing was sent".to_string(),
                            facts: Vec::new(),
                            table: None,
                            error: Some(error.message.to_string()),
                        });
                        return;
                    }
                };
                // The statements the request carried, read from the
                // request itself: the view describes what was sent, not
                // what the file says now.
                let statements: Vec<String> = request
                    .body
                    .as_ref()
                    .and_then(|body| body["statements"].as_array().cloned())
                    .map(|list| {
                        list.iter()
                            .filter_map(|statement| statement["sql"].as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                // The request says what it can commit, and the view
                // says what the failure means: a read-only query that
                // cannot reach the gateway is an unavailable gateway,
                // never a write that may have committed.
                let semantics = request.semantics;
                let response = match client::request_with_semantics(
                    &self.server,
                    &self.key,
                    request.method,
                    &request.path,
                    request.body,
                    None,
                    semantics,
                )
                .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        self.outcome = Some(Outcome {
                            headline: "the gateway did not answer".to_string(),
                            facts: Vec::new(),
                            table: None,
                            error: Some(error.message.to_string()),
                        });
                        return;
                    }
                };
                outcome_of(&invocation, &statements, &response)
            }
        };
        self.outcome = Some(outcome);
        self.refresh().await;
    }

    /// The database list with each database's traffic.
    pub fn databases(&self) -> Option<render::Table> {
        let list = self.list.as_ref()?;
        let status = self.status.clone().unwrap_or(Value::Null);
        // The contents are a separate read, and an absent one is not an
        // error: the traffic columns are still a real answer, and the
        // counts say "unknown" rather than the tab going blank.
        let contents = self.contents.clone().unwrap_or(Value::Null);
        Some(views::databases(list, &status, &contents))
    }

    /// The sorted database names the databases tab shows.
    ///
    /// The tab's Enter doorway prefills the palette from this list,
    /// so the doorway and the tab read the same names in the same
    /// order: the scrolled-to row and the prefilled name cannot
    /// disagree about which database is which.
    pub fn database_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .list
            .as_ref()
            .and_then(|list| list["databases"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|name| name.as_str().map(str::to_string))
            .collect();
        names.sort();
        names
    }

    /// The credential list.
    ///
    /// The tab strip counts what is in each tab, so it reads the same table
    /// the tab itself draws -- one source, so the count and the contents
    /// cannot disagree.
    pub fn keys_table(&self) -> Option<render::Table> {
        self.keys.as_ref().map(views::keys)
    }

    /// The statement shapes the service has run.
    ///
    /// The tab strip counts what is in each tab, so it reads the same table
    /// the tab itself draws -- one source, so the count and the contents
    /// cannot disagree.
    pub fn query_table(&self) -> Option<render::Table> {
        self.queries.as_ref().map(views::query_table)
    }

    /// Where each database lives on disk, and whether it is there.
    ///
    /// `None` when the gateway does not report its placement. The tab
    /// says so in words rather than showing an empty table, because an
    /// empty table would look like a gateway with no databases.
    pub fn locations(&self) -> Option<render::Table> {
        views::locations(self.status.as_ref()?)
    }

    /// How many rows of content the open tab can show at once.
    ///
    /// The body rectangle is known: the drawer sets it before it draws
    /// the tab, so this is the space the tab really has, not a guess.
    pub fn visible_rows(&self) -> usize {
        self.geometry
            .body
            .map(|body| (body.height as usize).saturating_sub(2))
            .filter(|rows| *rows > 0)
            .unwrap_or(1)
    }

    /// How many rows of a TABLE the open tab can show.
    ///
    /// A table spends two lines on the frame's borders, one on its
    /// header, and one more on the caption under its rows when it has
    /// one. Getting this wrong cuts the last row off the screen, which is
    /// the very thing the clamp exists to prevent.
    pub fn visible_table_rows(&self, caption: bool) -> usize {
        self.visible_rows()
            .saturating_sub(1 + usize::from(caption))
            .max(1)
    }

    /// Keep the scroll inside what the open tab holds.
    ///
    /// A tab that scrolls past its last row shows nothing, and an empty
    /// tab reads as "there is nothing here" -- a different and much worse
    /// claim than "you have reached the end". The offset also ran away
    /// without this: ten presses down on a two-row table needed ten
    /// presses up to come back, though one press had moved everything.
    ///
    /// `content` is how many rows the tab holds and `visible` how many it
    /// can show, so the last row stays on screen and a table that already
    /// fits does not scroll at all.
    pub fn clamp_scroll(&mut self, content: usize, visible: usize) {
        let max = content.saturating_sub(visible.max(1));
        self.scroll.clamp(self.focus, max);
    }
}

/// The outcome of listing one database's tables.
///
/// Pure so a test can read what the operator would read without a
/// gateway: the table is the shared [`views::database_tables`] reading.
pub fn tables_outcome(name: &str, response: &Value) -> Outcome {
    if let Some(error) = refusal(response) {
        return error;
    }
    Outcome {
        headline: format!("tables in {name}"),
        facts: vec![("database".to_string(), name.to_string())],
        table: Some(views::database_tables(response)),
        error: None,
    }
}

/// The outcome of reading one table's structure.
///
/// Pure so a test can read what the operator would read without a
/// gateway: the grid is the shared [`views::structure_columns`]
/// reading, and the facts say which table it is about.
pub fn schema_outcome(name: &str, table: Option<&str>, response: &Value) -> Outcome {
    if let Some(error) = refusal(response) {
        return error;
    }
    let structures = views::structures(response, table);
    if structures.is_empty() {
        let wanted = match table {
            Some(wanted) => format!("no table named {wanted} in {name}"),
            None => format!("{name} has no table to describe"),
        };
        return Outcome {
            headline: "there is no structure to show".to_string(),
            facts: vec![("database".to_string(), name.to_string())],
            table: None,
            error: Some(wanted),
        };
    }
    // A whole database is opened by its contents: one row per table.
    // One named table is opened by its own structure: the columns, their
    // types and their flags. The two are different questions, and the
    // grid answers the one that was asked.
    match table {
        None => {
            let rows: u64 = structures.iter().map(|entry| entry.rows).sum();
            Outcome {
                headline: format!("{name} holds {} tables", structures.len()),
                facts: vec![
                    ("database".to_string(), name.to_string()),
                    ("tables".to_string(), structures.len().to_string()),
                    ("rows".to_string(), rows.to_string()),
                ],
                table: Some(views::structure_contents(&structures)),
                error: None,
            }
        }
        Some(_) => {
            let first = &structures[0];
            Outcome {
                headline: format!("structure of {} in {name}", first.name),
                facts: views::structure_facts(first),
                table: Some(views::structure_columns(first)),
                error: None,
            }
        }
    }
}

/// The outcome of paging through one table's rows.
///
/// Pure for the same reason: the grid the operator pages is built
/// here, from the columns and rows the gateway returned, and the
/// headline says which slice of the table is on screen.
pub fn open_table_outcome(name: &str, table: &str, page: u64, response: &Value) -> Outcome {
    if let Some(error) = refusal(response) {
        return error;
    }
    let Some(columns) = response["columns"].as_array() else {
        return Outcome {
            headline: "the table could not be read".to_string(),
            facts: vec![("database".to_string(), name.to_string())],
            table: None,
            error: Some("the gateway answered without columns".to_string()),
        };
    };
    let rows: Vec<Vec<Value>> = response["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|row| row.as_array().cloned().unwrap_or_default())
        .collect();
    let names: Vec<String> = columns
        .iter()
        .map(|column| column.as_str().unwrap_or("").to_string())
        .collect();
    let first = page.saturating_mul(crate::tui::actions::TABLE_PAGE_SIZE);
    Outcome {
        headline: if rows.is_empty() {
            format!("{table} has no rows on page {page}")
        } else {
            format!("{table} rows {}–{}", first + 1, first + rows.len() as u64)
        },
        facts: vec![("database".to_string(), name.to_string())],
        table: Some(render::Table::from_json(&names, &rows)),
        error: None,
    }
}

/// The outcome of the rows browser: one page of a table, with the
/// footer that says which slice of the answer it is.
///
/// The footer is the browser's own, not the grid's: "rows 11-20 of 60"
/// is a statement about the whole answer under the current filter, and
/// only the browser knows that number. The grid shows the rows.
pub fn browser_outcome(browser: &Browser, response: &Value) -> Outcome {
    if let Some(error) = refusal(response) {
        return error;
    }
    let Some(columns) = response["columns"].as_array() else {
        return Outcome {
            headline: "the table could not be read".to_string(),
            facts: vec![
                ("database".to_string(), browser.database.clone()),
                ("table".to_string(), browser.table.clone()),
            ],
            table: None,
            error: Some("the gateway answered without columns".to_string()),
        };
    };
    let rows: Vec<Vec<Value>> = response["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|row| row.as_array().cloned().unwrap_or_default())
        .collect();
    let names: Vec<String> = columns
        .iter()
        .map(|column| column.as_str().unwrap_or("").to_string())
        .collect();
    let mut grid = render::Table::from_json(&names, &rows);
    grid.footer(browser.page_footer());
    Outcome {
        headline: format!("{} in {}", browser.table, browser.database),
        facts: vec![
            ("database".to_string(), browser.database.clone()),
            ("table".to_string(), browser.table.clone()),
        ],
        table: Some(grid),
        error: None,
    }
}

/// The outcome of a count: how many rows the current filter selects.
///
/// Read on its own so the page and the total cannot be confused: the
/// page answers "what is on screen", the count answers "how much is
/// there". One request could not answer both without the gateway
/// returning a second number nobody asked it for.
pub fn count_total(response: &Value) -> Option<u64> {
    response["rows"]
        .as_array()?
        .first()?
        .as_array()?
        .first()?
        .as_u64()
}

/// The refusal the gateway sent, read the way every outcome reads
/// one, so a refused table opens with the same words as any other
/// refused act.
fn refusal(response: &Value) -> Option<Outcome> {
    let error = response.get("error")?;
    let code = error["code"].as_str().unwrap_or("error");
    let message = error["message"].as_str().unwrap_or("refused");
    Some(Outcome {
        headline: "the gateway refused".to_string(),
        facts: Vec::new(),
        table: None,
        error: Some(format!("{code}: {message}")),
    })
}

/// Read the answer to an action.
///
/// The reading is the shared one: a batch's outcome comes from
/// [`views::batch_outcomes`], so the view says the same thing about a
/// failed batch that the CLI says.
fn outcome_of(invocation: &Invocation, statements: &[String], response: &Value) -> Outcome {
    if let Invocation::Tables { name } = invocation {
        return tables_outcome(name, response);
    }
    if let Invocation::Schema { name, table } = invocation {
        return schema_outcome(name, table.as_deref(), response);
    }
    if let Invocation::OpenTable { name, table, page } = invocation {
        return open_table_outcome(name, table, *page, response);
    }
    if !statements.is_empty() {
        let outcome = views::batch_outcomes(statements, response);
        let reason = outcome.reason();
        return Outcome {
            headline: if outcome.ran() {
                "the statements ran".to_string()
            } else {
                "the batch failed".to_string()
            },
            facts: Vec::new(),
            table: Some(outcome.table),
            error: reason,
        };
    }
    if let Some(columns) = response["columns"].as_array() {
        let rows: Vec<Vec<Value>> = response["rows"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|row| row.as_array().cloned().unwrap_or_default())
            .collect();
        let names: Vec<String> = columns
            .iter()
            .map(|column| column.as_str().unwrap_or("").to_string())
            .collect();
        return Outcome {
            headline: format!("{} rows", rows.len()),
            facts: Vec::new(),
            table: Some(render::Table::from_json(&names, &rows)),
            error: None,
        };
    }
    if let Some(error) = response.get("error") {
        let code = error["code"].as_str().unwrap_or("error");
        let message = error["message"].as_str().unwrap_or("refused");
        return Outcome {
            headline: "the gateway refused".to_string(),
            facts: Vec::new(),
            table: None,
            error: Some(format!("{code}: {message}")),
        };
    }
    let facts = match invocation {
        Invocation::CreateDatabase { name } => vec![("name".to_string(), name.clone())],
        Invocation::DeleteDatabase { name } => {
            let purge = response["purge_after_seconds"]
                .as_u64()
                .map(render::human_duration)
                .unwrap_or_else(|| "unknown".to_string());
            vec![
                ("name".to_string(), name.clone()),
                ("purges in".to_string(), purge),
            ]
        }
        Invocation::CloneDatabase {
            source,
            destination,
        } => vec![
            ("from".to_string(), source.clone()),
            ("to".to_string(), destination.clone()),
        ],
        Invocation::IssueKey {
            name,
            role,
            databases,
        } => vec![
            // The key is shown once and never again, so it is on the
            // screen, with the scope it is safe with.
            (
                "key".to_string(),
                response["key"].as_str().unwrap_or("").to_string(),
            ),
            ("name".to_string(), name.clone()),
            ("role".to_string(), role.clone()),
            ("scope".to_string(), databases.join(", ")),
        ],
        Invocation::RevokeKey { name } => vec![("name".to_string(), name.clone())],
        Invocation::RotateOwner => vec![
            (
                "key".to_string(),
                response["key"].as_str().unwrap_or("").to_string(),
            ),
            ("old key".to_string(), "dead immediately".to_string()),
        ],
        _ => Vec::new(),
    };
    Outcome {
        headline: match invocation {
            Invocation::CreateDatabase { name } => format!("created {name}"),
            Invocation::DeleteDatabase { name } => format!("tombstoned {name}"),
            Invocation::CloneDatabase {
                source,
                destination,
            } => format!("cloned {source} to {destination}"),
            Invocation::IssueKey { name, .. } => format!("issued {name}"),
            Invocation::RevokeKey { name } => format!("revoked {name}"),
            Invocation::RotateOwner => "rotated the owner credential".to_string(),
            _ => "done".to_string(),
        },
        facts,
        table: None,
        error: None,
    }
}

/// Run the view until the operator leaves it.
pub async fn run(server: &str, key: &str, options: Options) -> Result<()> {
    let mut app = App::new(server.to_string(), key.to_string());
    let (events, mut inbox) = tokio::sync::mpsc::unbounded_channel();
    // crossterm's reader blocks, so it lives on its own thread and the
    // loop waits for its events beside the refresh.
    std::thread::spawn(move || {
        while let Ok(event) = ratatui::crossterm::event::read() {
            // The channel is closed when the view has left; the send
            // failing is the thread's signal to stop.
            if events.send(event).is_err() {
                break;
            }
        }
    });
    let mut screen = screen::CrosstermScreen::new().map_err(|_| Error::internal())?;
    screen.enter().map_err(|_| Error::internal())?;
    // The guard gives the terminal back however this ends: a normal
    // exit, an error, or a panic in a draw function.
    let mut guard = screen::Guard::new(screen);
    // The first tick is a full period away: the view reads the service
    // once when it opens, and the ticker takes it from there. A ticker
    // that fired at once would read everything twice.
    let period = Duration::from_secs(options.refresh_secs.clamp(1, 3600));
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first read happens now, so the view opens with facts rather
    // than with an empty screen.
    app.refresh().await;
    loop {
        if guard.draw(|frame| draw::draw(frame, &mut app)).is_err() {
            return Err(Error::internal());
        }
        tokio::select! {
            event = inbox.recv() => {
                match event {
                    Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        app.on_key(key);
                        if app.quit {
                            break;
                        }
                        if let Some(invocation) = app.take_pending() {
                            app.run_invocation(invocation).await;
                        }
                    }
                    Some(Event::Mouse(mouse)) => {
                        app.on_mouse(mouse);
                        // A click can queue an action -- opening a
                        // database -- and it runs like any other.
                        if let Some(invocation) = app.take_pending() {
                            app.run_invocation(invocation).await;
                        }
                    }
                    Some(_) => {}
                    // The reader thread is gone: there is nothing left
                    // to wait for.
                    None => break,
                }
            }
            _ = ticker.tick() => {
                app.refresh().await;
            }
        }
    }
    guard.release().map_err(|_| Error::internal())?;
    Ok(())
}
