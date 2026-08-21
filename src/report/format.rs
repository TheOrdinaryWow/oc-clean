//! Shared human-output formatting: sizes, percentages, colored headings, and aligned tables.
//!
//! Every human renderer routes through this module so a column width, a unit suffix, or a
//! color decision is defined once. Color is resolved from the caller's terminal facts rather
//! than probed here, which keeps rendering deterministic under test.

use std::io::Write;

use comfy_table::{Cell, CellAlignment, ContentArrangement, Table, presets};
use owo_colors::OwoColorize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::error::Error;

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// Column width reserved for a scalar field's label.
///
/// Wide enough for the longest label any report emits, so no label ever runs into its value.
pub const LABEL_WIDTH: usize = 30;

/// Formats a byte count using binary units, matching `du -h` and `ls -h` conventions.
///
/// The raw byte count is intentionally omitted: it is noise in a human report, and the exact
/// value remains available in the `--json` output for anything that needs arithmetic.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn bytes(value: u64) -> String {
    let mut scaled = value as f64;
    let mut unit = 0;
    while scaled >= 1024.0 && unit < UNITS.len() - 1 {
        scaled /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{scaled:.2} {}", UNITS[unit])
    }
}

/// Formats an optional byte count, naming the absent case explicitly.
#[must_use]
pub fn optional_bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "not present".to_owned(), bytes)
}

/// Returns `value` as a percentage of `total`, treating an empty total as zero percent.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn percent(value: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        value as f64 * 100.0 / total as f64
    }
}

/// Formats a percentage with the fixed precision used across every report.
#[must_use]
pub fn percent_of(value: u64, total: u64) -> String {
    format!("{:.2}%", percent(value, total))
}

/// Formats a millisecond epoch timestamp as a compact UTC date and time.
///
/// Falls back to the raw value when the timestamp is outside the representable range, which
/// keeps a corrupt row visible instead of silently rendering a wrong date.
#[must_use]
pub fn timestamp_ms(value: i64) -> String {
    let seconds = value.div_euclid(1_000);
    civil_utc(seconds).map_or_else(
        || value.to_string(),
        |(year, month, day, hour, minute)| {
            format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}Z")
        },
    )
}

/// Truncates `text` to `limit` terminal columns, replacing control characters.
///
/// Control characters are stripped because a session title is user-authored data that can
/// carry a newline or an escape sequence, either of which would corrupt an aligned table.
///
/// The budget is measured in display columns rather than characters, so a CJK title occupies
/// the same width on screen as a Latin one at the same limit.
#[must_use]
pub fn sanitize(text: &str, limit: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.width() <= limit {
        return trimmed.to_owned();
    }

    // One column is reserved for the ellipsis that marks the truncation.
    let budget = limit.saturating_sub(1);
    let mut truncated = String::new();
    let mut used = 0;
    for character in trimmed.chars() {
        let width = character.width().unwrap_or(0);
        if used + width > budget {
            break;
        }
        truncated.push(character);
        used += width;
    }
    truncated.push('…');
    truncated
}

/// Shortens an identifier to a recognizable prefix.
///
/// The full identifier stays in the `--json` report, so the human report only needs enough
/// characters to tell two sessions apart at a glance.
#[must_use]
pub fn short_id(id: &str) -> String {
    const KEPT: usize = 12;

    if id.chars().count() <= KEPT {
        return id.to_owned();
    }
    let mut shortened: String = id.chars().take(KEPT).collect();
    shortened.push('…');
    shortened
}

/// Terminal capabilities that decide whether output carries ANSI styling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Style {
    color: bool,
}

impl Style {
    /// Resolves styling from the output stream's terminal state and the `NO_COLOR` convention.
    #[must_use]
    pub const fn resolve(stdout_is_terminal: bool, no_color_is_set: bool) -> Self {
        Self {
            color: stdout_is_terminal && !no_color_is_set,
        }
    }

    /// Returns a style that never emits ANSI sequences.
    #[must_use]
    pub const fn plain() -> Self {
        Self { color: false }
    }

    /// Reports whether ANSI styling is active.
    #[must_use]
    pub const fn is_colored(self) -> bool {
        self.color
    }

    /// Renders a section heading.
    #[must_use]
    pub fn heading(self, title: &str) -> String {
        if self.color {
            title.cyan().bold().to_string()
        } else {
            title.to_owned()
        }
    }

    /// Renders a table header cell.
    #[must_use]
    pub fn header(self, label: &str) -> String {
        if self.color {
            label.bold().to_string()
        } else {
            label.to_owned()
        }
    }

    /// Renders a de-emphasized value such as an identifier or a timestamp.
    #[must_use]
    pub fn dim(self, text: &str) -> String {
        if self.color {
            text.dimmed().to_string()
        } else {
            text.to_owned()
        }
    }

    /// Renders a passing verdict.
    #[must_use]
    pub fn pass(self, text: &str) -> String {
        if self.color {
            text.green().bold().to_string()
        } else {
            text.to_owned()
        }
    }

    /// Renders a failing verdict.
    #[must_use]
    pub fn fail(self, text: &str) -> String {
        if self.color {
            text.red().bold().to_string()
        } else {
            text.to_owned()
        }
    }

    /// Renders a verdict that needs attention without being a failure.
    #[must_use]
    pub fn warn(self, text: &str) -> String {
        if self.color {
            text.yellow().bold().to_string()
        } else {
            text.to_owned()
        }
    }
}

/// Column alignment within an aligned, borderless table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Align {
    Left,
    Right,
}

impl From<Align> for CellAlignment {
    fn from(value: Align) -> Self {
        match value {
            Align::Left => Self::Left,
            Align::Right => Self::Right,
        }
    }
}

/// A borderless table whose column widths are computed from the content it holds.
///
/// Fixed `{:<25}` padding is what produced the overlapping columns this replaces: any value
/// wider than its slot pushed every later column out of alignment.
pub struct Grid {
    table: Table,
}

impl Grid {
    /// Creates a table with the given headers and per-column alignment.
    #[must_use]
    pub fn new(style: Style, headers: &[(&str, Align)]) -> Self {
        let mut table = Table::new();
        table
            .load_style(presets::NOTHING)
            .set_content_arrangement(ContentArrangement::Dynamic);
        table.set_header(
            headers
                .iter()
                .map(|(label, align)| {
                    Cell::new(style.header(label)).set_alignment(CellAlignment::from(*align))
                })
                .collect::<Vec<_>>(),
        );
        for (index, (_, align)) in headers.iter().enumerate() {
            if let Some(column) = table.column_mut(index) {
                column.set_cell_alignment(CellAlignment::from(*align));
                // No left padding, so a grid's first column lines up with the scalar fields
                // written above it; the gap between columns lives on the right instead.
                column.set_padding((0, 2));
            }
        }
        Self { table }
    }

    /// Appends one row, which must have the same arity as the headers.
    pub fn row(&mut self, cells: Vec<String>) {
        self.table.add_row(cells);
    }

    /// Reports whether the table holds no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.table.row_count() == 0
    }

    /// Writes the rendered table, indented to sit under its heading.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when writing fails.
    pub fn write(&self, output: &mut dyn Write) -> Result<(), Error> {
        for line in self.table.to_string().lines() {
            writeln!(output, "  {}", line.trim_end()).map_err(io_error)?;
        }
        Ok(())
    }
}

/// Writes a blank line followed by a styled section heading.
///
/// # Errors
///
/// Returns [`Error::Io`] when writing fails.
pub fn heading(output: &mut dyn Write, title: &str, style: Style) -> Result<(), Error> {
    writeln!(output, "\n{}", style.heading(title)).map_err(io_error)
}

/// Writes one indented `label  value` line for a single scalar fact.
///
/// # Errors
///
/// Returns [`Error::Io`] when writing fails.
pub fn field(
    output: &mut dyn Write,
    label: &str,
    value: impl std::fmt::Display,
) -> Result<(), Error> {
    writeln!(output, "  {label:<LABEL_WIDTH$}{value}").map_err(io_error)
}

/// Writes an indented bullet line.
///
/// # Errors
///
/// Returns [`Error::Io`] when writing fails.
pub fn bullet(output: &mut dyn Write, text: &str) -> Result<(), Error> {
    writeln!(output, "  - {text}").map_err(io_error)
}

/// Writes an indented note explaining a caveat of the section above it.
///
/// # Errors
///
/// Returns [`Error::Io`] when writing fails.
pub fn note(output: &mut dyn Write, text: &str, style: Style) -> Result<(), Error> {
    writeln!(output, "  {}", style.dim(text)).map_err(io_error)
}

fn io_error(source: std::io::Error) -> Error {
    Error::Io {
        path: std::path::PathBuf::from("<stdout>"),
        source,
    }
}

/// Converts a Unix timestamp in seconds into UTC civil date and time fields.
///
/// This is the civil-from-days algorithm; it avoids a date-library dependency for what is
/// only ever used to render a report line.
#[allow(clippy::many_single_char_names)]
fn civil_utc(seconds: i64) -> Option<(i64, u32, u32, u32, u32)> {
    let days = seconds.div_euclid(86_400);
    let remainder = seconds.rem_euclid(86_400);
    let hour = u32::try_from(remainder / 3_600).ok()?;
    let minute = u32::try_from((remainder % 3_600) / 60).ok()?;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * shifted_month + 2) / 5 + 1).ok()?;
    let month = u32::try_from(if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    })
    .ok()?;
    let year = if month <= 2 { year + 1 } else { year };
    Some((year, month, day, hour, minute))
}

#[cfg(test)]
mod tests {
    use super::{
        Align, Grid, Style, bytes, optional_bytes, percent, percent_of, sanitize, short_id,
        timestamp_ms,
    };

    #[test]
    fn sizes_render_binary_units_without_the_raw_byte_count() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1_024), "1.00 KiB");
        assert_eq!(bytes(43_242_958_848), "40.27 GiB");
        for value in [1_024_u64, 1_048_576, 43_242_958_848] {
            assert!(
                !bytes(value).contains("bytes"),
                "the raw count belongs in JSON, not the human report"
            );
        }
    }

    #[test]
    fn an_absent_size_is_named_rather_than_rendered_as_zero() {
        assert_eq!(optional_bytes(None), "not present");
        assert_eq!(optional_bytes(Some(2_048)), "2.00 KiB");
    }

    #[test]
    fn an_empty_total_is_zero_percent_instead_of_a_division_by_zero() {
        assert!((percent(5, 0) - 0.0).abs() < f64::EPSILON);
        assert_eq!(percent_of(1, 4), "25.00%");
    }

    #[test]
    fn control_characters_never_reach_an_aligned_table() {
        assert_eq!(sanitize("first\nsecond", 40), "first second");
        assert_eq!(sanitize("  padded  ", 40), "padded");
        assert_eq!(sanitize("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn truncation_budgets_terminal_columns_rather_than_characters() {
        // Each ideograph occupies two columns, so a limit of 7 fits three of them plus the
        // ellipsis. A character-based budget would have kept six and doubled the column use.
        assert_eq!(sanitize("性能问题分析", 7), "性能问…");
        assert_eq!(sanitize("性能", 7), "性能");
    }

    #[test]
    fn a_short_identifier_is_left_intact() {
        assert_eq!(short_id("ses_1"), "ses_1");
        assert_eq!(short_id("ses_001431de5ffeU1DZc3KoFD4AzW8"), "ses_001431de…");
    }

    #[test]
    fn timestamps_render_as_utc_dates() {
        assert_eq!(timestamp_ms(0), "1970-01-01 00:00Z");
        assert_eq!(timestamp_ms(1_700_000_000_000), "2023-11-14 22:13Z");
    }

    #[test]
    fn a_wide_value_does_not_push_later_columns_out_of_alignment() {
        let mut grid = Grid::new(
            Style::plain(),
            &[("Object", Align::Left), ("Size", Align::Right)],
        );
        assert!(grid.is_empty());
        grid.row(vec![
            "event_aggregate_type_seq_idx".to_owned(),
            "185.66 MiB".to_owned(),
        ]);
        grid.row(vec!["event".to_owned(), "35.47 GiB".to_owned()]);

        let mut rendered = Vec::new();
        grid.write(&mut rendered).expect("writing cannot fail");
        let text = String::from_utf8(rendered).expect("output is UTF-8");
        let widths: Vec<usize> = text
            .lines()
            .map(|line| line.find("MiB").or_else(|| line.find("GiB")).unwrap_or(0))
            .filter(|offset| *offset > 0)
            .collect();

        assert!(!grid.is_empty());
        assert_eq!(widths.len(), 2);
        assert_eq!(
            widths[0], widths[1],
            "right-aligned sizes must share one column edge"
        );
        assert!(!text.contains('\u{1b}'), "plain style emits no ANSI");
    }

    #[test]
    fn plain_style_never_emits_ansi_while_colored_style_does() {
        let plain = Style::plain();
        assert!(!plain.is_colored());
        for rendered in [
            plain.heading("Row Counts"),
            plain.header("Table"),
            plain.dim("ses_1"),
            plain.pass("PASS"),
            plain.fail("FAIL"),
            plain.warn("WARN"),
        ] {
            assert!(!rendered.contains('\u{1b}'));
        }

        let colored = Style::resolve(true, false);
        assert!(colored.is_colored());
        assert!(colored.heading("Row Counts").contains('\u{1b}'));
        assert!(!Style::resolve(true, true).is_colored());
        assert!(!Style::resolve(false, false).is_colored());
    }
}
