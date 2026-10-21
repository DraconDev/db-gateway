//! Terminal rendering for the `dg` CLI.
//!
//! One renderer, used by every human-facing command and by the
//! interactive shell, so the CLI looks the same everywhere and the
//! "what is going on" question has one answer.
//!
//! Three properties this module owns, and the reasons behind them:
//!
//! * **The machine surface is untouched.** `--json` and piped stdout stay
//!   byte-identical JSON. Decoration only ever happens on the table path,
//!   which the commands reach only when stdout is a terminal.
//! * **Layout is measured, not counted.** Column widths come from the
//!   display width of the text (`unicode-width`), not its character count,
//!   so a CJK value or an emoji in a cell does not shear the grid.
//! * **Colour is decoration, never information.** A value that is red
//!   reads the same without colour, because the colour is a second
//!   rendering of a state word that is already spelled out ("fresh",
//!   "stale", "unknown"). `NO_COLOR` and a dumb terminal turn it off.

use std::borrow::Cow;
use std::io::Write;
use unicode_width::UnicodeWidthStr;

use crate::error::{Error, Result};

/// ANSI escapes. Kept as constants so the palette is auditable in one
/// place rather than scattered as literals.
mod ansi {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const CYAN: &str = "\x1b[36m";
}

/// How output is decorated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Emit ANSI colour.
    pub color: bool,
    /// Use box-drawing characters rather than ASCII.
    pub unicode: bool,
}

impl Theme {
    /// The theme for a stream that may or may not be a terminal.
    ///
    /// Colour needs a terminal to be interpreted and a reason to exist, so
    /// it is on only for a terminal, and only when the environment allows
    /// it. The character set follows the locale, so a non-UTF-8 terminal
    /// gets an ASCII grid rather than mojibake.
    pub fn for_stream(is_terminal: bool) -> Self {
        Self {
            color: is_terminal && color_allowed(),
            unicode: unicode_allowed(),
        }
    }

    /// An exactly-specified theme. Used by tests, which must not depend
    /// on the environment they run in.
    pub fn fixed(color: bool, unicode: bool) -> Self {
        Self { color, unicode }
    }
}

/// Colour is off when `NO_COLOR` is set (https://no-color.org) or the
/// terminal is dumb. Everything else gets colour.
fn color_allowed() -> bool {
    color_allowed_with(std::env::var_os("NO_COLOR"), std::env::var("TERM").ok())
}

/// The colour rule, with the environment passed in rather than read, so
/// the rule itself is testable without mutating the process's
/// environment.
fn color_allowed_with(no_color: Option<std::ffi::OsString>, term: Option<String>) -> bool {
    if no_color.is_some_and(|value| !value.is_empty()) {
        return false;
    }
    !matches!(term.as_deref(), Some("dumb"))
}

/// Unicode box drawing when the locale says UTF-8. An unset locale is
/// treated as UTF-8 (the overwhelming modern default); a locale that is
/// explicitly non-UTF-8 (C, POSIX) gets ASCII so the grid still lines up
/// in a terminal that cannot render the box characters.
fn unicode_allowed() -> bool {
    unicode_allowed_with([
        std::env::var("LC_ALL").ok(),
        std::env::var("LC_CTYPE").ok(),
        std::env::var("LANG").ok(),
    ])
}

/// The character-set rule, with the locales passed in rather than read.
fn unicode_allowed_with(locales: [Option<String>; 3]) -> bool {
    for locale in locales.into_iter().flatten() {
        if locale.is_empty() {
            continue;
        }
        let upper = locale.to_ascii_uppercase();
        return upper.contains("UTF-8") || upper.contains("UTF8");
    }
    true
}

/// A text style. Every style changes how a value reads, never what it
/// says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Plain,
    /// Secondary information: keys, borders, counts.
    Dim,
    /// Headings.
    Bold,
    /// Column headers.
    Header,
    /// A failure state.
    Red,
    /// A healthy state.
    Green,
    /// A state that needs attention.
    Yellow,
    /// A value that identifies something (a name, a path).
    Cyan,
}

impl Style {
    fn escape(self) -> &'static str {
        match self {
            Style::Plain => "",
            Style::Dim => ansi::DIM,
            Style::Bold => ansi::BOLD,
            Style::Header => "\x1b[1;36m",
            Style::Red => ansi::RED,
            Style::Green => ansi::GREEN,
            Style::Yellow => ansi::YELLOW,
            Style::Cyan => ansi::CYAN,
        }
    }
}

/// Column alignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// One cell of a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cell {
    /// Ordinary text.
    Text(String),
    /// A number. Rendered with the column's alignment.
    Number(String),
    /// A SQL NULL. Distinct from the text "NULL" by being dim, and by
    /// the value itself never being a string.
    Null,
    /// Text with an explicit style.
    Styled(String, Style),
    /// A sparkline: a row of values drawn as a bar chart in one line.
    ///
    /// Its own kind because it is measured by its DISPLAY WIDTH: the
    /// bars are one column each whatever the values are.
    Sparkline(Vec<u64>),
}

impl Cell {
    /// The text and style this cell renders as.
    ///
    /// The text is escaped here, at the one point where a cell's
    /// contents become terminal bytes. Every view -- a query table, a
    /// dashboard section, the interactive shell -- draws its cells
    /// through this function, so a value from the wire cannot repaint
    /// the terminal in any of them.
    pub fn parts(&self) -> (Cow<'_, str>, Style) {
        match self {
            Cell::Text(text) => (sanitize(text), Style::Plain),
            Cell::Number(text) => (sanitize(text), Style::Plain),
            Cell::Null => (Cow::Borrowed("NULL"), Style::Dim),
            Cell::Styled(text, style) => (sanitize(text), *style),
            Cell::Sparkline(values) => (Cow::Owned(sparkline(values)), Style::Cyan),
        }
    }

    /// A text cell.
    pub fn text(value: impl Into<String>) -> Self {
        Cell::Text(value.into())
    }

    /// A number cell.
    pub fn number(value: impl ToString) -> Self {
        Cell::Number(value.to_string())
    }

    /// A cell styled by meaning: a state word, a name, a path.
    pub fn styled(value: impl Into<String>, style: Style) -> Self {
        Cell::Styled(value.into(), style)
    }
}

/// A table of columns and rows.
#[derive(Clone, Debug, Default)]
pub struct Table {
    columns: Vec<(String, Align)>,
    rows: Vec<Vec<Cell>>,
    footer: Option<String>,
}

impl Table {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a column. The alignment applies to every row of that column.
    ///
    /// The header is escaped, because a column name can come from the
    /// wire (a query's result columns) and the header is drawn like any
    /// other cell.
    pub fn column(&mut self, header: impl Into<String>, align: Align) -> &mut Self {
        self.columns.push((sanitize_owned(&header.into()), align));
        self
    }

    /// Add a row. Cells beyond the column count are dropped, and a
    /// short row renders its missing cells as empty -- a row shorter than
    /// the header is untrusted wire data, never a panic.
    pub fn row(&mut self, cells: Vec<Cell>) -> &mut Self {
        self.rows.push(cells);
        self
    }

    /// A line printed under the table, such as a row count.
    ///
    /// Escaped like a cell: a footer can carry a name from the wire.
    pub fn footer(&mut self, text: impl Into<String>) -> &mut Self {
        self.footer = Some(sanitize_owned(&text.into()));
        self
    }

    /// Build a table from a JSON result: `columns` names the columns and
    /// `rows` holds the values. A column whose values are all numbers is
    /// right-aligned, so figures line up under their headers; a column
    /// holding any non-number stays left-aligned, because a column of
    /// mostly-numbers with one text value is text.
    pub fn from_json(columns: &[String], rows: &[Vec<serde_json::Value>]) -> Self {
        let mut table = Table::new();
        let numeric: Vec<bool> = (0..columns.len())
            .map(|index| {
                let mut seen = false;
                for row in rows {
                    match row.get(index) {
                        Some(serde_json::Value::Null) | None => {}
                        Some(serde_json::Value::Number(_)) => seen = true,
                        Some(_) => return false,
                    }
                }
                seen
            })
            .collect();
        for (index, header) in columns.iter().enumerate() {
            let align = if numeric[index] {
                Align::Right
            } else {
                Align::Left
            };
            table.column(header.clone(), align);
        }
        for row in rows {
            let cells = (0..columns.len())
                .map(|index| match row.get(index) {
                    // A row shorter than the header is malformed wire
                    // data. It renders as an empty cell rather than as
                    // NULL: no value was sent, and claiming a SQL NULL
                    // would be a different statement.
                    None => Cell::Text(String::new()),
                    Some(serde_json::Value::Null) => Cell::Null,
                    Some(serde_json::Value::Number(number)) => Cell::Number(number.to_string()),
                    // The value is NOT escaped here. Escaping belongs to
                    // the renderer, at the point where a cell becomes
                    // terminal bytes, so that every view is covered by
                    // the one rule rather than by each command's
                    // diligence.
                    Some(serde_json::Value::String(text)) => Cell::Text(text.clone()),
                    Some(serde_json::Value::Bool(flag)) => Cell::Text(flag.to_string()),
                    Some(other) => Cell::Text(other.to_string()),
                })
                .collect();
            table.row(cells);
        }
        table
    }

    /// The column headers and their alignment.
    ///
    /// Read access for a second painter: the terminal UI draws the same
    /// table this module writes, so it needs the model's shape without
    /// re-deriving it from the JSON.
    pub fn columns(&self) -> &[(String, Align)] {
        &self.columns
    }

    /// The rows, each a list of cells.
    pub fn rows(&self) -> &[Vec<Cell>] {
        &self.rows
    }

    /// The line printed under the table, if there is one.
    pub fn footer_text(&self) -> Option<&str> {
        self.footer.as_deref()
    }

    /// Write the table.
    pub fn write(&self, out: &mut dyn Write, theme: &Theme) -> Result<()> {
        let count = self.columns.len();
        if count == 0 {
            return Ok(());
        }
        let widths = self.widths();
        self.rule(out, theme, Rule::Top)?;
        self.header(out, theme, &widths)?;
        self.rule(out, theme, Rule::Middle)?;
        for row in &self.rows {
            self.body(out, theme, &widths, row)?;
        }
        self.rule(out, theme, Rule::Bottom)?;
        if let Some(footer) = &self.footer {
            let text = paint(footer, Style::Dim, theme);
            writeln!(out, "{text}").map_err(io)?;
        }
        Ok(())
    }

    /// The display width of every column.
    fn widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = self
            .columns
            .iter()
            .map(|(header, _)| header.width())
            .collect();
        for row in &self.rows {
            for (index, cell) in row.iter().enumerate() {
                if index < widths.len() {
                    let (text, _) = cell.parts();
                    widths[index] = widths[index].max(text.width());
                }
            }
        }
        widths
    }

    /// The header row.
    fn header(&self, out: &mut dyn Write, theme: &Theme, widths: &[usize]) -> Result<()> {
        let mut line = String::new();
        for (index, (header, _)) in self.columns.iter().enumerate() {
            let width = widths.get(index).copied().unwrap_or(0);
            line.push_str(&self.vertical(theme));
            line.push(' ');
            line.push_str(&paint(header, Style::Header, theme));
            let used = header.width();
            line.push_str(&" ".repeat(width.saturating_sub(used)));
            line.push(' ');
        }
        line.push_str(&self.vertical(theme));
        writeln!(out, "{line}").map_err(io)
    }

    /// One body row.
    fn body(
        &self,
        out: &mut dyn Write,
        theme: &Theme,
        widths: &[usize],
        row: &[Cell],
    ) -> Result<()> {
        let mut line = String::new();
        for (index, (_, align)) in self.columns.iter().enumerate() {
            let width = widths.get(index).copied().unwrap_or(0);
            let cell = row.get(index);
            let (text, style) = cell.map_or((Cow::Borrowed(""), Style::Dim), Cell::parts);
            line.push_str(&self.vertical(theme));
            line.push(' ');
            let used = text.width();
            let pad = width.saturating_sub(used);
            match align {
                Align::Left => {
                    line.push_str(&paint(&text, style, theme));
                    line.push_str(&" ".repeat(pad));
                }
                Align::Right => {
                    line.push_str(&" ".repeat(pad));
                    line.push_str(&paint(&text, style, theme));
                }
            }
            line.push(' ');
        }
        line.push_str(&self.vertical(theme));
        writeln!(out, "{line}").map_err(io)
    }

    /// A horizontal rule.
    fn rule(&self, out: &mut dyn Write, theme: &Theme, kind: Rule) -> Result<()> {
        let widths = self.widths();
        let fill = if theme.unicode { "─" } else { "-" };
        let mut line = String::new();
        for (index, width) in widths.iter().enumerate() {
            if index > 0 {
                line.push_str(&paint(self.joint(kind, theme), Style::Dim, theme));
            }
            line.push_str(&paint(&fill.repeat(width + 2), Style::Dim, theme));
        }
        let mut rendered = String::new();
        rendered.push_str(&paint(
            self.corner(kind, Corner::Left, theme),
            Style::Dim,
            theme,
        ));
        rendered.push_str(&line);
        rendered.push_str(&paint(
            self.corner(kind, Corner::Right, theme),
            Style::Dim,
            theme,
        ));
        writeln!(out, "{rendered}").map_err(io)
    }

    /// The vertical border.
    fn vertical(&self, theme: &Theme) -> String {
        paint(if theme.unicode { "│" } else { "|" }, Style::Dim, theme)
    }

    /// A rule joint.
    fn joint(&self, kind: Rule, theme: &Theme) -> &'static str {
        match (kind, theme.unicode) {
            (Rule::Top, true) => "┬",
            (Rule::Middle, true) => "┼",
            (Rule::Bottom, true) => "┴",
            (_, false) => "+",
        }
    }

    /// A corner.
    fn corner(&self, kind: Rule, side: Corner, theme: &Theme) -> &'static str {
        if !theme.unicode {
            return "+";
        }
        match (kind, side) {
            (Rule::Top, Corner::Left) => "┌",
            (Rule::Top, Corner::Right) => "┐",
            (Rule::Middle, Corner::Left) => "├",
            (Rule::Middle, Corner::Right) => "┤",
            (Rule::Bottom, Corner::Left) => "└",
            (Rule::Bottom, Corner::Right) => "┘",
        }
    }
}

/// Which side of a rule a corner is on.
#[derive(Clone, Copy)]
enum Corner {
    Left,
    Right,
}

/// Which rule is being drawn.
#[derive(Clone, Copy)]
enum Rule {
    Top,
    Middle,
    Bottom,
}

/// Apply a style to text, if the theme allows colour.
fn paint(text: &str, style: Style, theme: &Theme) -> String {
    if !theme.color || style == Style::Plain {
        return text.to_string();
    }
    format!("{}{}{}", style.escape(), text, ansi::RESET)
}

/// Escape the control characters in a value that came from the wire.
///
/// The gateway is reached over HTTP, and the CLI accepts any `--server`
/// origin including plain http, so a cell's contents are untrusted. A
/// raw ESC or newline in a cell would let a value repaint the terminal
/// or forge a row of the table; escaping the control characters keeps a
/// value inside its own cell. The escape is visible (`\u{1b}`), so
/// nothing is silently swallowed.
///
/// This is the renderer's single escaping rule, applied at the moment
/// text becomes terminal bytes -- in [`Cell::parts`] and in the table's
/// column, footer and section setters. A command cannot forget it: a
/// cell reaches the screen only through this function, whether the
/// value came from a JSON row, a database name, a key name or a
/// service version.
fn sanitize(text: &str) -> Cow<'_, str> {
    if !text.chars().any(char::is_control) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .flat_map(|c| {
                if c.is_control() {
                    c.escape_default().collect::<Vec<_>>()
                } else {
                    vec![c]
                }
            })
            .collect(),
    )
}

/// Escape a value that is kept, not just drawn.
fn sanitize_owned(text: &str) -> String {
    sanitize(text).into_owned()
}

/// Escape a value that came from the wire, for a caller outside this
/// module that draws it itself.
///
/// The same rule as the cells: the gateway is untrusted, so a control
/// character in a value must not reach the terminal raw.
pub fn escape_text(text: &str) -> String {
    sanitize_owned(text)
}

/// A result a command reports to a person.
///
/// A command that does something answers with a headline (what
/// happened) and, when there is more to say, a set of facts about it.
/// A command that reports a list answers with a table.
///
/// This is the shape every command's human output takes, so a reader
/// learns one format instead of one per command.
pub enum Outcome {
    /// Something happened: a headline, and the facts that matter.
    Done {
        /// What happened, in a few words: "created game_a".
        headline: String,
        /// The facts behind the headline, if there are any.
        facts: Vec<(String, String)>,
    },
    /// Nothing to report: the command did what was asked and there is
    /// nothing more to say.
    Quiet,
    /// A list of things, under a heading.
    List {
        /// What the list is: "databases".
        title: String,
        /// The rows, already shaped for the table.
        table: Table,
    },
}

impl Outcome {
    /// A headline on its own.
    pub fn done(headline: impl Into<String>) -> Self {
        Outcome::Done {
            headline: headline.into(),
            facts: Vec::new(),
        }
    }

    /// A headline with facts behind it.
    pub fn with_facts(headline: impl Into<String>, facts: Vec<(String, String)>) -> Self {
        Outcome::Done {
            headline: headline.into(),
            facts,
        }
    }

    /// A list of things.
    pub fn list(title: impl Into<String>, table: Table) -> Self {
        Outcome::List {
            title: title.into(),
            table,
        }
    }

    /// Draw the outcome.
    pub fn write(&self, out: &mut dyn Write, theme: &Theme) -> Result<()> {
        match self {
            Outcome::Done { headline, facts } => {
                writeln!(out, "{}", paint(headline, Style::Green, theme)).map_err(io)?;
                if !facts.is_empty() {
                    let width = facts.iter().map(|(key, _)| key.width()).max().unwrap_or(0);
                    for (key, value) in facts {
                        writeln!(
                            out,
                            "  {}{}  {}",
                            paint(key, Style::Dim, theme),
                            " ".repeat(width.saturating_sub(key.width())),
                            paint(value, Style::Plain, theme)
                        )
                        .map_err(io)?;
                    }
                }
                Ok(())
            }
            Outcome::Quiet => Ok(()),
            Outcome::List { title, table } => {
                writeln!(out, "{}", paint(title, Style::Bold, theme)).map_err(io)?;
                table.write(out, theme)
            }
        }
    }
}


/// How long ago a timestamp was, as a person reads it.
///
/// The absolute time is kept alongside, because "3m ago" alone does not
/// say when a write happened, only how recently.
pub fn human_epoch_ago(epoch: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(epoch);
    let age = now.saturating_sub(epoch);
    format!("{} ago", human_duration(age))
}

/// A block of a section: a list of fields, or a table.
pub enum Block {
    /// Key/value pairs, rendered as an indented two-column list.
    Fields(Vec<(String, Cell)>),
    /// A table, rendered under the section's title.
    Table(Table),
}

/// A titled group of related information.
///
/// A dashboard reads as a set of titled groups rather than one long
/// list of `property | value` rows, because the groups are what the
/// reader is actually asking about: what the service is, how it is
/// admitting work, what it has been asked for, and what each database
/// is doing.
pub struct Section {
    title: String,
    blocks: Vec<Block>,
}

impl Section {
    /// A titled section.
    ///
    /// The title is escaped, because it is drawn like any other text.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: sanitize_owned(&title.into()),
            blocks: Vec::new(),
        }
    }

    /// Add a key/value field.
    ///
    /// The key is escaped, like every other text the renderer draws.
    pub fn field(&mut self, key: impl Into<String>, value: Cell) -> &mut Self {
        let fields = match self.blocks.last_mut() {
            Some(Block::Fields(fields)) => fields,
            _ => {
                self.blocks.push(Block::Fields(Vec::new()));
                match self.blocks.last_mut() {
                    Some(Block::Fields(fields)) => fields,
                    _ => unreachable!("the block just pushed is a field list"),
                }
            }
        };
        fields.push((sanitize_owned(&key.into()), value));
        self
    }

    /// Add a table under this section's title.
    pub fn table(&mut self, table: Table) -> &mut Self {
        self.blocks.push(Block::Table(table));
        self
    }

    /// The section's title.
    ///
    /// Read access for a second painter, like `Table::columns`.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The section's blocks: field lists and tables, in the order they
    /// were added.
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Write the section.
    pub fn write(&self, out: &mut dyn Write, theme: &Theme) -> Result<()> {
        if self.blocks.is_empty() {
            return Ok(());
        }
        writeln!(out, "{}", paint(&self.title, Style::Bold, theme)).map_err(io)?;
        for block in &self.blocks {
            match block {
                Block::Fields(fields) => self.write_fields(out, theme, fields)?,
                Block::Table(table) => table.write(out, theme)?,
            }
        }
        Ok(())
    }

    /// Write a list of key/value fields, with the keys dim and the
    /// values aligned into a column of their own.
    fn write_fields(
        &self,
        out: &mut dyn Write,
        theme: &Theme,
        fields: &[(String, Cell)],
    ) -> Result<()> {
        let width = fields.iter().map(|(key, _)| key.width()).max().unwrap_or(0);
        for (key, cell) in fields {
            let (text, style) = cell.parts();
            let pad = width.saturating_sub(key.width());
            writeln!(
                out,
                "  {}{}  {}",
                paint(key, Style::Dim, theme),
                " ".repeat(pad),
                paint(&text, style, theme)
            )
            .map_err(io)?;
        }
        Ok(())
    }
}

/// Write a dashboard: titled sections separated by a blank line.
pub fn write_sections(out: &mut dyn Write, theme: &Theme, sections: &[Section]) -> Result<()> {
    for (index, section) in sections.iter().enumerate() {
        if index > 0 {
            writeln!(out).map_err(io)?;
        }
        section.write(out, theme)?;
    }
    Ok(())
}

/// The two wait figures a per-database view shows.
///
/// The p95 and the max are DIFFERENT facts: the p95 says how bad the
/// queue usually is, the max says how bad it ever got. They are built
/// together, in one place, so that a view cannot put the wrong one
/// under a header -- which is the defect this replaces, where a
/// maximum was drawn under a "p95" label.
pub struct WaitFigures {
    /// The p95 of that database's own recent admission waits.
    pub p95: Cell,
    /// The longest single wait that database has seen.
    pub max: Cell,
}

/// The wait figures of one database, from its `/v1/status` entry.
///
/// `absent` is what to draw when the database has never waited: a
/// percentile of nothing is nothing, and a zero would claim a
/// measurement that was never made. The views draw it differently (a
/// table cell says "-", a field says "no waits recorded"), so the
/// wording is the caller's.
pub fn wait_figures(entry: &serde_json::Value, absent: &str) -> WaitFigures {
    let cell = |key: &str| match entry[key].as_u64() {
        Some(micros) => Cell::text(human_micros(micros)),
        None => Cell::text(absent),
    };
    WaitFigures {
        p95: cell("queue_wait_p95_us"),
        max: cell("queue_wait_max_us"),
    }
}

/// The wait figures of one database, each paired with the label that
/// names it.
///
/// A view that draws them as labelled fields takes them from here, so
/// the label and the value travel together and cannot be swapped at
/// the call site.
pub fn wait_figure_fields(entry: &serde_json::Value, absent: &str) -> Vec<(&'static str, Cell)> {
    let figures = wait_figures(entry, absent);
    vec![("wait p95", figures.p95), ("wait max", figures.max)]
}

/// A count per minute, as a person reads it.
///
/// A rate is read as "how much per minute", so a count over a window
/// is scaled to that unit. `span_secs` is how long the window actually
/// observed: a service up for ten seconds has observed ten seconds, and
/// its rate is scaled from those ten rather than from a minute it never
/// lived.
pub fn human_rate(count: u64, span_secs: u64) -> String {
    let span = span_secs.max(1);
    let per_minute = count as f64 * 60.0 / span as f64;
    format!("{}/min", human_count(per_minute as u64, ""))
}

/// A duration in microseconds, as a person reads it.
///
/// A wait that is shorter than a millisecond is still a wait, and
/// rounding it to "0 ms" would read as "no wait at all".
pub fn human_micros(micros: u64) -> String {
    if micros < 1_000 {
        return format!("{micros} us");
    }
    if micros < 10_000 {
        return format!("{:.1} ms", micros as f64 / 1_000.0);
    }
    if micros < 1_000_000 {
        return format!("{} ms", micros / 1_000);
    }
    format!("{:.1} s", micros as f64 / 1_000_000.0)
}

/// A count as a person reads it: `1,204` with a thousands separator,
/// and a suffix for the unit it is counted in.
pub fn human_count(value: u64, unit: &str) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    if unit.is_empty() {
        grouped
    } else {
        format!("{grouped} {unit}")
    }
}

/// A proportion as a person reads it: `0.006%`.
///
/// A ratio below a thousandth is worth more precision than a ratio
/// above it, so the number of decimal places follows the magnitude
/// rather than being fixed.
pub fn human_percent(numerator: u64, denominator: u64) -> String {
    if denominator == 0 {
        return "-".to_string();
    }
    let ratio = numerator as f64 / denominator as f64 * 100.0;
    if ratio == 0.0 {
        return "0%".to_string();
    }
    if ratio < 0.01 {
        // Below a hundredth of a percent, a fixed precision would print
        // "0.00%", which reads as "no errors" when there were some.
        format!("{ratio:.4}%")
    } else if ratio < 1.0 {
        format!("{ratio:.3}%")
    } else if ratio < 10.0 {
        format!("{ratio:.2}%")
    } else {
        format!("{ratio:.1}%")
    }
}

/// A row of values as a sparkline: a bar chart in one line.
///
/// The values are scaled against the largest of them, so the shape of
/// the row is what the reader sees. A row of zeros is a row of spaces:
/// a flat line, not a chart of nothing.
pub fn sparkline(values: &[u64]) -> String {
    const LEVELS: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let peak = values.iter().copied().max().unwrap_or(0);
    if peak == 0 {
        return " ".repeat(values.len());
    }
    values
        .iter()
        .map(|value| {
            // The height of the bar, from the peak down to nothing.
            let eighths = ((*value as f64 / peak as f64) * (LEVELS.len() - 1) as f64).round();
            LEVELS[(eighths as usize).min(LEVELS.len() - 1)]
        })
        .collect()
}

/// A byte count as a person reads it.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A duration in seconds as a person reads it.
pub fn human_duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let rest = seconds % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {rest}s")
    } else {
        format!("{rest}s")
    }
}

/// A Unix timestamp as a person reads it: `2026-10-19 23:18 UTC`.
pub fn human_epoch(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let rest = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rest / 3_600,
        (rest % 3_600) / 60
    )
}

/// A backup-set stamp (`20261019T231511Z`) as a person reads it:
/// `2026-10-19 23:15`.
///
/// The stamp comes from the wire, so it is checked before it is
/// interpreted. The shape is validated on the BYTES: a string slice at
/// a non-character boundary panics, and a malformed stamp is not worth
/// a crash. A stamp that is not exactly `YYYYMMDDTHHMM` in ASCII is
/// passed through unchanged for the reader to make sense of.
pub fn human_set_stamp(stamp: &str) -> String {
    let bytes = stamp.as_bytes();
    if bytes.len() < 13 || bytes[8] != b'T' {
        return stamp.to_string();
    }
    // The date and time parts must be ASCII digits, and the separator
    // must be `T`. Checking the bytes is what makes the slicing below
    // safe: a multi-byte character anywhere in the prefix would put a
    // slice point inside a character.
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    if !digits(0..8) || !digits(9..13) {
        return stamp.to_string();
    }
    // The slices are now known to be on character boundaries.
    let year = &stamp[0..4];
    let month = &stamp[4..6];
    let day = &stamp[6..8];
    let hour = &stamp[9..11];
    let minute = &stamp[11..13];
    format!("{year}-{month}-{day} {hour}:{minute}")
}

/// The civil date for a count of days since the Unix epoch. This is
/// Howard Hinnant's `civil_from_days`, which is exact for the whole
/// range of a 64-bit day count and needs no calendar dependency.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted / 146_097
    } else {
        (shifted - 146_096) / 146_097
    };
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

fn io(_: std::io::Error) -> Error {
    Error::new(500, "output_failed", "failed to write output")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_table(columns: &[(&str, Align)], rows: &[Vec<Cell>]) -> String {
        let mut table = Table::new();
        for (header, align) in columns {
            table.column(*header, *align);
        }
        for row in rows {
            table.row(row.clone());
        }
        let mut out = Vec::new();
        table.write(&mut out, &Theme::fixed(false, true)).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_table_has_borders_headers_and_rows() {
        let text = plain_table(
            &[("id", Align::Right), ("name", Align::Left)],
            &[vec![Cell::number(1), Cell::text("aria")]],
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "┌────┬──────┐");
        assert_eq!(lines[1], "│ id │ name │");
        assert_eq!(lines[2], "├────┼──────┤");
        assert_eq!(lines[3], "│  1 │ aria │");
        assert_eq!(lines[4], "└────┴──────┘");
    }

    #[test]
    fn numbers_are_right_aligned_under_their_header() {
        let text = plain_table(
            &[("n", Align::Right)],
            &[vec![Cell::number(1)], vec![Cell::number(1000)]],
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[3], "│    1 │");
        assert_eq!(lines[4], "│ 1000 │");
    }

    #[test]
    fn null_is_distinct_from_the_text_null() {
        let text = plain_table(
            &[("v", Align::Left)],
            &[vec![Cell::Null], vec![Cell::text("NULL")]],
        );
        let lines: Vec<&str> = text.lines().collect();
        // Same spelling, different style: the NULL cell is dim, the text
        // cell is not. With colour off they read alike, which is why the
        // value is a JSON null in the first place and never a string.
        assert_eq!(lines[3], "│ NULL │");
        assert_eq!(lines[4], "│ NULL │");
    }

    #[test]
    fn a_short_row_renders_empty_and_a_long_row_is_truncated() {
        let text = plain_table(
            &[("a", Align::Left), ("b", Align::Left), ("c", Align::Left)],
            &[
                vec![Cell::text("1")],
                vec![Cell::text("1"), Cell::text("2"), Cell::text("3")],
            ],
        );
        let lines: Vec<&str> = text.lines().collect();
        // A short row renders its missing cells as empty rather than
        // panicking: a row is untrusted wire data.
        assert_eq!(lines[3], "│ 1 │   │   │");
        // An over-long row is truncated to the header's column count.
        assert_eq!(lines[4], "│ 1 │ 2 │ 3 │");
    }

    #[test]
    fn wide_characters_are_measured_by_display_width() {
        // Each of these is two columns wide, so the grid must not shear.
        let text = plain_table(
            &[("cjk", Align::Left), ("n", Align::Right)],
            &[vec![Cell::text("日本語"), Cell::number(7)]],
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "┌────────┬───┐");
        assert_eq!(lines[1], "│ cjk    │ n │");
        assert_eq!(lines[3], "│ 日本語 │ 7 │");
        // Every row is the same display width: the table lines up.
        let widths: Vec<usize> = lines.iter().map(|l| l.width()).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{lines:?}");
    }

    #[test]
    fn ascii_theme_draws_a_plain_grid() {
        let mut table = Table::new();
        table.column("id", Align::Right);
        table.row(vec![Cell::number(1)]);
        let mut out = Vec::new();
        table.write(&mut out, &Theme::fixed(false, false)).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // The horizontal fill must be ASCII too: a box-drawing dash in an
        // ASCII table is a character the terminal may not have.
        assert_eq!(lines[0], "+----+");
        assert_eq!(lines[1], "| id |");
        assert_eq!(lines[3], "|  1 |");
        assert!(
            !text.contains('─'),
            "unicode fill in an ascii table: {text:?}"
        );
    }

    #[test]
    fn colour_is_off_without_colour_support() {
        let mut table = Table::new();
        table.column("id", Align::Right);
        table.row(vec![Cell::number(1)]);
        let mut out = Vec::new();
        table.write(&mut out, &Theme::fixed(false, true)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains('\u{1b}'), "colour leaked: {text:?}");
    }

    #[test]
    fn colour_wraps_the_value_only() {
        let mut table = Table::new();
        table.column("state", Align::Left);
        table.row(vec![Cell::styled("fresh", Style::Green)]);
        let mut out = Vec::new();
        table.write(&mut out, &Theme::fixed(true, true)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\u{1b}[32mfresh\u{1b}[0m"), "{text:?}");
        assert!(text.contains("\u{1b}[1;36mstate\u{1b}[0m"), "{text:?}");
    }

    #[test]
    fn json_tables_infer_alignment_from_the_values() {
        let rows = vec![
            vec![serde_json::json!(1), serde_json::json!("aria")],
            vec![serde_json::json!(2), serde_json::json!("octavia")],
        ];
        let table = Table::from_json(&["id".into(), "name".into()], &rows);
        let mut out = Vec::new();
        table.write(&mut out, &Theme::fixed(false, true)).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[1], "│ id │ name    │");
        assert_eq!(lines[3], "│  1 │ aria    │");
    }

    #[test]
    fn a_json_column_with_one_text_value_stays_left_aligned() {
        let rows = vec![vec![serde_json::json!(1)], vec![serde_json::json!("two")]];
        let table = Table::from_json(&["v".into()], &rows);
        let mut out = Vec::new();
        table.write(&mut out, &Theme::fixed(false, true)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("│ 1   │"), "{text}");
        assert!(text.contains("│ two │"), "{text}");
    }

    #[test]
    fn bytes_are_human_readable() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(16_384), "16.0 KiB");
        assert_eq!(human_bytes(1024 * 1024 * 3), "3.0 MiB");
    }

    #[test]
    fn durations_are_human_readable() {
        assert_eq!(human_duration(0), "0s");
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(90), "1m 30s");
        assert_eq!(human_duration(3_600), "1h 0m");
        assert_eq!(human_duration(90_000), "1d 1h");
    }

    #[test]
    fn epochs_are_human_readable() {
        assert_eq!(human_epoch(0), "1970-01-01 00:00 UTC");
        assert_eq!(human_epoch(1_000_000_000), "2001-09-09 01:46 UTC");
        assert_eq!(human_epoch(1_795_000_000), "2026-11-18 11:06 UTC");
        // A leap day, to catch an off-by-one in the civil conversion.
        assert_eq!(human_epoch(1_709_164_800), "2024-02-29 00:00 UTC");
    }

    #[test]
    fn backup_set_stamps_are_human_readable() {
        assert_eq!(human_set_stamp("20261019T231511Z"), "2026-10-19 23:15");
        // Anything that is not a stamp is passed through untouched.
        assert_eq!(human_set_stamp("latest"), "latest");
        assert_eq!(human_set_stamp("2026-10-19"), "2026-10-19");
    }

    #[test]
    fn a_malformed_stamp_is_never_interpreted() {
        // The stamp comes from the wire, so it may be anything at all. A
        // multi-byte character before a slice point used to panic the CLI
        // ("byte index 4 is not a char boundary") because the shape was
        // checked on the bytes but the string was sliced anyway.
        for hostile in [
            "日本abT123456",
            "\u{1b}[31mT123456",
            "2026-10-19T23:15",
            "aaaaTaaaa",
            "",
            "T",
        ] {
            // The contract is that it does not panic, and that a value
            // that is not a well-formed stamp is shown as it arrived.
            let shown = human_set_stamp(hostile);
            if hostile == "日本abT123456" || hostile == "\u{1b}[31mT123456" {
                assert_eq!(shown, hostile, "a malformed stamp was rewritten");
            }
        }
        // A stamp whose date is fine but whose separator is not is not a
        // stamp, and is not rewritten into one.
        assert_eq!(human_set_stamp("20261019X231511Z"), "20261019X231511Z");
        // A well-formed stamp is still humanised.
        assert_eq!(human_set_stamp("20261019T231511Z"), "2026-10-19 23:15");
    }

    #[test]
    fn a_sparkline_draws_the_shape_of_its_values() {
        // The sparkline is scaled against its own peak, so the shape is
        // what the reader sees: a rising row rises.
        let line = sparkline(&[0, 1, 2, 3, 4]);
        assert_eq!(line.chars().count(), 5, "one bar per value: {line:?}");
        assert!(line.starts_with(' '), "the zero bar is flat: {line:?}");
        assert!(line.ends_with('█'), "the peak is the tallest bar: {line:?}");
        // A row of nothing is a flat line, not a chart of noise.
        assert_eq!(sparkline(&[0, 0, 0]), "   ");
        // An empty row is empty.
        assert_eq!(sparkline(&[]), "");
        // Every bar is one column wide, whatever the values are.
        let wide = sparkline(&[u64::MAX, 1]);
        assert_eq!(wide.chars().count(), 2);
    }

    #[test]
    fn a_sparkline_is_a_cell_and_keeps_the_grid_square() {
        let mut table = Table::new();
        table.column("activity", Align::Left);
        table.row(vec![Cell::Sparkline(vec![0, 1, 2, 3])]);
        let mut out = Vec::new();
        table.write(&mut out, &Theme::fixed(false, false)).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // The row is as wide as the header plus the bars, and the
        // borders match.
        assert!(lines[3].starts_with('|'), "no row: {text:?}");
        assert_eq!(lines[3].chars().count(), lines[1].chars().count());
    }

    #[test]
    fn rates_are_scaled_to_the_window_that_was_observed() {
        // Ten requests in ten seconds is one per minute, not ten.
        assert_eq!(human_rate(10, 10), "60/min");
        // Ten requests in a minute is ten per minute.
        assert_eq!(human_rate(10, 60), "10/min");
        // A service up for a second reports its one request honestly.
        assert_eq!(human_rate(1, 1), "60/min");
        // A zero window does not divide by zero.
        assert_eq!(human_rate(5, 0), "300/min");
        // A large rate is grouped.
        assert_eq!(human_rate(1_204_800, 60), "1,204,800/min");
    }

    #[test]
    fn a_short_wait_is_still_a_wait() {
        // A wait shorter than a millisecond must not read as "no wait".
        assert_eq!(human_micros(400), "400 us");
        assert_eq!(human_micros(1_500), "1.5 ms");
        assert_eq!(human_micros(12_000), "12 ms");
        assert_eq!(human_micros(1_400_000), "1.4 s");
    }

    #[test]
    fn counts_are_grouped_for_reading() {
        assert_eq!(human_count(0, ""), "0");
        assert_eq!(human_count(999, ""), "999");
        assert_eq!(human_count(1000, ""), "1,000");
        assert_eq!(human_count(1234567, ""), "1,234,567");
        assert_eq!(human_count(42, "rows"), "42 rows");
    }

    #[test]
    fn percentages_keep_the_precision_the_ratio_needs() {
        // Three errors in 48,213 requests: a small ratio is worth the
        // digits, because "0.00%" would read as "no errors".
        assert_eq!(human_percent(3, 48_213), "0.0062%");
        assert_eq!(human_percent(0, 100), "0%");
        assert_eq!(human_percent(1, 2), "50.0%");
        assert_eq!(human_percent(1, 100), "1.00%");
        // No denominator means no ratio, not a zero.
        assert_eq!(human_percent(0, 0), "-");
    }

    #[test]
    fn a_cell_never_carries_a_raw_control_character() {
        // The renderer's escaping rule: whatever a cell holds, the bytes
        // that reach the terminal carry no control character of their
        // own. This is the one rule that protects every view, so it is
        // tested on the renderer rather than on each command.
        let hostile = "evil\u{1b}[31mRED\u{1b}[0m\u{1b}[2J";
        let mut table = Table::new();
        table.column(hostile, Align::Left);
        table.row(vec![Cell::styled(hostile, Style::Cyan)]);
        table.footer(hostile);
        let mut out = Vec::new();
        table.write(&mut out, &Theme::fixed(false, false)).unwrap();
        let text = String::from_utf8(out).unwrap();
        // The value is shown, escaped: nothing is swallowed.
        assert!(text.contains("evil"), "the value was dropped: {text:?}");
        assert!(
            text.contains("\\u{1b}"),
            "the value was not escaped: {text:?}"
        );
        // And no raw control byte reached the output. With colour off
        // the renderer emits no escape sequences of its own, so every
        // control character here would have come from the value -- the
        // only exception being the line break the renderer writes itself.
        for c in text.chars().filter(|c| *c != '\n') {
            assert!(
                !c.is_control(),
                "raw control character in the output: {text:?}"
            );
        }
    }

    #[test]
    fn a_value_adds_no_escape_bytes_to_the_ones_the_renderer_uses() {
        // With colour on, the renderer emits its own escape sequences, so
        // "no control character at all" is not the contract. The contract
        // is that the VALUE adds none: a table holding a hostile value
        // has exactly as many escape bytes as the same table holding a
        // benign one of the same shape.
        let count_escapes = |value: &str| {
            let mut table = Table::new();
            table.column("database", Align::Left);
            table.row(vec![Cell::styled(value, Style::Cyan)]);
            let mut out = Vec::new();
            table.write(&mut out, &Theme::fixed(true, true)).unwrap();
            String::from_utf8(out)
                .unwrap()
                .chars()
                .filter(|c| *c == '\u{1b}')
                .count()
        };
        let benign = count_escapes("good_name");
        let hostile = count_escapes("evil\u{1b}[31mRED\u{1b}[0m\u{1b}[2J");
        assert_eq!(
            benign, hostile,
            "the value contributed escape bytes to the terminal"
        );
    }

    #[test]
    fn a_section_never_carries_a_raw_control_character() {
        // The same rule for a dashboard section: a title, a key and a
        // value are all drawn text.
        let hostile = "evil\u{1b}[31mRED";
        let mut section = Section::new(hostile);
        section.field(hostile, Cell::styled(hostile, Style::Cyan));
        let mut out = Vec::new();
        section
            .write(&mut out, &Theme::fixed(false, false))
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        for c in text.chars().filter(|c| *c != '\n') {
            assert!(
                !c.is_control(),
                "raw control character in the output: {text:?}"
            );
        }
    }

    #[test]
    fn no_color_disables_colour_and_a_dumb_terminal_disables_it_too() {
        // The documented environment rules, exercised directly.
        let no_color = |value: &str| Some(std::ffi::OsString::from(value));
        let term = |value: &str| Some(value.to_string());
        assert!(color_allowed_with(None, term("xterm-256color")));
        assert!(color_allowed_with(None, None));
        // NO_COLOR set to anything non-empty turns colour off.
        assert!(!color_allowed_with(no_color("1"), term("xterm")));
        assert!(!color_allowed_with(no_color("true"), term("xterm")));
        // An empty NO_COLOR is not a request to disable colour.
        assert!(color_allowed_with(no_color(""), term("xterm")));
        // A dumb terminal cannot interpret colour.
        assert!(!color_allowed_with(None, term("dumb")));
    }


    #[test]
    fn the_wait_figures_pair_the_p95_with_the_p95() {
        // The pairing is built in one place, so both views that draw
        // it agree about which figure is which.
        let entry = serde_json::json!({
            "queue_wait_p95_us": 1_000,
            "queue_wait_max_us": 900_000,
        });
        let figures = wait_figures(&entry, "-");
        assert_eq!(figures.p95.parts().0, "1.0 ms");
        assert_eq!(figures.max.parts().0, "900 ms");

        // A database that has never waited says so rather than
        // claiming a measurement that was never made.
        let idle = wait_figures(&serde_json::json!({}), "no waits recorded");
        assert_eq!(idle.p95.parts().0, "no waits recorded");
        assert_eq!(idle.max.parts().0, "no waits recorded");
    }

    #[test]
    fn a_non_utf8_locale_selects_the_ascii_grid() {
        // The first locale that is set decides, in the order the
        // environment reports them.
        assert!(unicode_allowed_with([some("en_US.UTF-8"), None, None]));
        assert!(unicode_allowed_with([some("en_GB.utf8"), None, None]));
        assert!(unicode_allowed_with([None, some("C.UTF-8"), None]));
        // An explicit non-UTF-8 locale gets ASCII.
        assert!(!unicode_allowed_with([some("C"), None, None]));
        assert!(!unicode_allowed_with([some("POSIX"), None, None]));
        // An empty locale is skipped, and the next one decides.
        assert!(unicode_allowed_with([some(""), some("en_US.UTF-8"), None]));
        // Nothing set at all is treated as UTF-8.
        assert!(unicode_allowed_with([None, None, None]));
        fn some(value: &str) -> Option<String> {
            Some(value.to_string())
        }
    }
}
