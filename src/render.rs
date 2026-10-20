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
    /// The theme for output that is going to a terminal: colour and
    /// Unicode are both allowed, subject to the environment.
    pub fn terminal() -> Self {
        Self {
            color: color_allowed(),
            unicode: unicode_allowed(),
        }
    }

    /// The theme for output that is not going to a terminal: no colour
    /// (there is no terminal to interpret it), same character set.
    pub fn plain() -> Self {
        Self {
            color: false,
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
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    !matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
}

/// Unicode box drawing when the locale says UTF-8. An unset locale is
/// treated as UTF-8 (the overwhelming modern default); a locale that is
/// explicitly non-UTF-8 (C, POSIX) gets ASCII so the grid still lines up
/// in a terminal that cannot render the box characters.
fn unicode_allowed() -> bool {
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(value) = std::env::var(key) {
            if value.is_empty() {
                continue;
            }
            let upper = value.to_ascii_uppercase();
            return upper.contains("UTF-8") || upper.contains("UTF8");
        }
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
}

impl Cell {
    /// The text and style this cell renders as.
    pub fn parts(&self) -> (&str, Style) {
        match self {
            Cell::Text(text) => (text, Style::Plain),
            Cell::Number(text) => (text, Style::Plain),
            Cell::Null => ("NULL", Style::Dim),
            Cell::Styled(text, style) => (text, *style),
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
    pub fn column(&mut self, header: impl Into<String>, align: Align) -> &mut Self {
        self.columns.push((header.into(), align));
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
    pub fn footer(&mut self, text: impl Into<String>) -> &mut Self {
        self.footer = Some(text.into());
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
                    Some(serde_json::Value::Null) | None => Cell::Null,
                    Some(serde_json::Value::Number(number)) => Cell::Number(number.to_string()),
                    Some(serde_json::Value::String(text)) => Cell::Text(text.clone()),
                    Some(serde_json::Value::Bool(flag)) => Cell::Text(flag.to_string()),
                    Some(other) => Cell::Text(other.to_string()),
                })
                .collect();
            table.row(cells);
        }
        table
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
            let (text, style) = cell.map_or((&""[..], Style::Dim), Cell::parts);
            line.push_str(&self.vertical(theme));
            line.push(' ');
            let used = text.width();
            let pad = width.saturating_sub(used);
            match align {
                Align::Left => {
                    line.push_str(&paint(text, style, theme));
                    line.push_str(&" ".repeat(pad));
                }
                Align::Right => {
                    line.push_str(&" ".repeat(pad));
                    line.push_str(&paint(text, style, theme));
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
        let mut line = String::new();
        for (index, width) in widths.iter().enumerate() {
            if index > 0 {
                line.push_str(&paint(self.joint(kind, theme), Style::Dim, theme));
            }
            line.push_str(&paint(&"─".repeat(width + 2), Style::Dim, theme));
        }
        let mut rendered = String::new();
        rendered.push_str(&paint(self.corner(kind, Corner::Left, theme), Style::Dim, theme));
        rendered.push_str(&line);
        rendered.push_str(&paint(self.corner(kind, Corner::Right, theme), Style::Dim, theme));
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

/// A section of key/value lines: a heading, then indented pairs.
pub struct Section {
    title: String,
    rows: Vec<(String, Cell)>,
}

impl Section {
    /// A titled section.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            rows: Vec::new(),
        }
    }

    /// Add a key/value pair.
    pub fn row(&mut self, key: impl Into<String>, value: Cell) -> &mut Self {
        self.rows.push((key.into(), value));
        self
    }

    /// Write the section: a heading, then the pairs with the keys dim
    /// and aligned so the values form a column of their own.
    pub fn write(&self, out: &mut dyn Write, theme: &Theme) -> Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let width = self
            .rows
            .iter()
            .map(|(key, _)| key.width())
            .max()
            .unwrap_or(0);
        writeln!(out, "  {}", paint(&self.title, Style::Bold, theme)).map_err(io)?;
        for (key, cell) in &self.rows {
            let (text, style) = cell.parts();
            let pad = width.saturating_sub(key.width());
            writeln!(
                out,
                "    {}{}  {}",
                paint(key, Style::Dim, theme),
                " ".repeat(pad),
                paint(text, style, theme)
            )
            .map_err(io)?;
        }
        Ok(())
    }
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
pub fn human_set_stamp(stamp: &str) -> String {
    let bytes = stamp.as_bytes();
    if bytes.len() < 13 || bytes[8] != b'T' {
        return stamp.to_string();
    }
    let year = &stamp[0..4];
    let month = &stamp[4..6];
    let day = &stamp[6..8];
    let hour = &stamp[9..11];
    let minute = &stamp[11..13];
    if !year.bytes().all(|b| b.is_ascii_digit())
        || !month.bytes().all(|b| b.is_ascii_digit())
        || !day.bytes().all(|b| b.is_ascii_digit())
        || !hour.bytes().all(|b| b.is_ascii_digit())
        || !minute.bytes().all(|b| b.is_ascii_digit())
    {
        return stamp.to_string();
    }
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
            &[vec![Cell::text("1")], vec![Cell::text("1"), Cell::text("2"), Cell::text("3")]],
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[3], "│ 1       │");
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
        assert_eq!(lines[0], "┌──────┬────┐");
        assert_eq!(lines[1], "│ cjk  │  n │");
        assert_eq!(lines[3], "│ 日本語 │   7 │");
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
        assert_eq!(text.lines().next().unwrap(), "+----+");
        assert!(text.contains("| id |"), "{text}");
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
        let rows = vec![
            vec![serde_json::json!(1)],
            vec![serde_json::json!("two")],
        ];
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
        assert_eq!(human_epoch(1_795_000_000), "2026-11-14 03:06 UTC");
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
}
