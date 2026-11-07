//! Selecting text inside one pane and copying it.
//!
//! A selection is anchored to the lines of the pane's text rather than to screen rows, so it stays
//! on the same text while a busy log scrolls on or drops its oldest lines.
//! What it copies is drawn again off screen exactly as the pane draws it, so it matches what
//! the pane shows, wrapped or not.

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::{Paragraph, Widget as _};

use super::{PopupContent, PopupHit};
use crate::render::rows_u16;
use crate::{App, InfoView};

/// Fills the cells of an off-screen render that no text was drawn into, so copying tells
/// padding and the hidden half of a wide character apart from a real space.
const UNWRITTEN: &str = "\0";

/// A column past the end of every line, where a whole-line selection ends.
pub(crate) const END_OF_LINE: u16 = u16::MAX;

/// A pane whose text can be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextPane {
    Logs,
    Healthcheck,
    Help,
    Info(InfoView),
}

/// A place on screen in a pane's text: a rendered row counted from the top of everything the
/// pane can scroll through, and a cell column, or [`END_OF_LINE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TextPos {
    pub(crate) row: usize,
    pub(crate) col: u16,
}

/// A place in a pane's text that survives its lines shifting: the line, by a key that stays with
/// it, the rendered row within the line, and a cell column, or [`END_OF_LINE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextAnchor {
    /// A log line's sequence number, or the line's index in a pane that never drops lines.
    line: u64,
    row_in_line: usize,
    col: u16,
}

/// What a pane's rows belong to; a selection made under one context means nothing under another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextContext {
    pub(crate) pane: TextPane,
    /// The service whose text the pane shows, if any.
    service: Option<String>,
    /// How the logs are formatted, which decides what their lines say.
    format: Option<crate::json_log::LineFormat>,
    wrap: bool,
    width: u16,
}

/// Selected text: the cells from `anchor` to `head` in text order, both included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) context: TextContext,
    anchor: TextAnchor,
    head: TextAnchor,
}

/// A selection placed on the rows its pane draws now.
#[derive(Debug, Clone, Copy)]
struct Placed {
    start: TextPos,
    end: TextPos,
}

impl Placed {
    /// The selected columns of `row`, both ends included, the last possibly [`END_OF_LINE`].
    fn columns_in_row(self, row: usize) -> Option<(u16, u16)> {
        let Self { start, end } = self;
        if start == end || row < start.row || row > end.row {
            return None;
        }
        let from = if row == start.row { start.col } else { 0 };
        let to = if row == end.row { end.col } else { END_OF_LINE };
        Some((from, to))
    }
}

/// The last column of row `y` of `buf` that shows text, counting the hidden half of a wide
/// character there, or `None` for a row without any.
///
/// Blanks after the text are padding, such as the spaces a service's terminal output fills its
/// rows with, so a selection neither highlights nor copies them.
fn text_end(buf: &Buffer, y: u16) -> Option<u16> {
    (0..buf.area.width).rev().find_map(|x| {
        let symbol = buf.cell((x, y))?.symbol();
        (symbol != UNWRITTEN && !symbol.trim().is_empty()).then(|| {
            let width = ratatui::text::Span::raw(symbol).width();
            x.saturating_add(rows_u16(width.saturating_sub(1)))
        })
    })
}

/// The text one rendered row shows: each grapheme with the column it starts at.
type RowCells = Vec<(usize, String)>;

/// The cells of row `y` of `buf` that hold text.
fn row_cells(buf: &Buffer, y: u16) -> RowCells {
    (0..buf.area.width)
        .filter_map(|x| {
            let symbol = buf.cell((x, y))?.symbol();
            (symbol != UNWRITTEN).then(|| (usize::from(x), symbol.to_string()))
        })
        .collect()
}

/// The graphemes of `line` that a pane draws into cells, leaving out control and zero-width
/// ones.
fn drawn_graphemes(line: &ratatui::text::Line<'_>) -> Vec<String> {
    line.styled_graphemes(ratatui::style::Style::default())
        .map(|grapheme| grapheme.symbol)
        .filter(|symbol| {
            !symbol.contains(char::is_control) && ratatui::text::Span::raw(*symbol).width() > 0
        })
        .map(str::to_string)
        .collect()
}

/// The cells of `line` drawn on one row that never wraps, however far past the pane it runs.
fn unwrapped_cells(line: &ratatui::text::Line<'_>) -> RowCells {
    let mut col = 0;
    drawn_graphemes(line)
        .into_iter()
        .map(|symbol| {
            let at = col;
            col += ratatui::text::Span::raw(symbol.as_str()).width();
            (at, symbol)
        })
        .collect()
}

/// The last column of `cells` that shows text, counting the hidden half of a wide character,
/// or `None` for a row without any.
///
/// Blanks after the text are padding, such as the spaces a service's terminal output fills its
/// rows with, so a selection neither highlights nor copies them.
fn cells_text_end(cells: &RowCells) -> Option<usize> {
    cells
        .iter()
        .rev()
        .find(|(_, symbol)| !symbol.trim().is_empty())
        .map(|(col, symbol)| col + ratatui::text::Span::raw(symbol.as_str()).width().max(1) - 1)
}

/// The text of `cells` from column `from` to `to`, both included.
fn cells_between(cells: &RowCells, from: usize, to: usize) -> String {
    cells
        .iter()
        .filter(|(col, _)| (from..=to).contains(col))
        .map(|(_, symbol)| symbol.as_str())
        .collect()
}

/// A selected column as a cell column, with [`END_OF_LINE`] past every line.
fn column(col: u16) -> usize {
    if col == END_OF_LINE {
        usize::MAX
    } else {
        usize::from(col)
    }
}

impl App {
    /// Where `pane`'s text was drawn in the last frame.
    pub(crate) fn text_area(&self, pane: TextPane) -> Option<Rect> {
        match pane {
            TextPane::Logs => Some(crate::render::log_view::text_area(self.hits.logs)),
            TextPane::Healthcheck => Some(crate::render::log_view::text_area(self.hits.health)),
            TextPane::Help | TextPane::Info(_) => match &self.hits.popup {
                Some(PopupHit {
                    content:
                        PopupContent::Text {
                            pane: shown, area, ..
                        },
                    ..
                }) if *shown == pane => Some(*area),
                _ => None,
            },
        }
    }

    /// What `pane`'s rows belong to now, or `None` when the pane is not on screen.
    pub(crate) fn text_context(&self, pane: TextPane) -> Option<TextContext> {
        let area = self.text_area(pane).filter(|area| !area.is_empty())?;
        let current = self.state.current_service();
        let service = current.map(|service| service.snapshot.id.clone());
        let (service, format, wrap) = match pane {
            TextPane::Logs => (
                Some(service?),
                current.map(|service| service.line_format(self.pretty_json_logs)),
                self.log_view.wrap,
            ),
            TextPane::Healthcheck => (Some(service?), None, self.healthcheck_view.wrap),
            TextPane::Info(InfoView::Service) => (service, None, false),
            TextPane::Help | TextPane::Info(InfoView::Session) => (None, None, false),
        };
        Some(TextContext {
            pane,
            service,
            format,
            wrap,
            width: area.width,
        })
    }

    /// The row drawn at the top of `pane`.
    pub(crate) fn first_row(&self, pane: TextPane) -> usize {
        match pane {
            TextPane::Logs => self.log_view.scroll_offset,
            TextPane::Healthcheck => self.healthcheck_view.scroll_offset,
            TextPane::Help | TextPane::Info(_) => self.popup_scroll,
        }
    }

    /// The lines of `pane`'s text and the rows they are drawn on, or `None` for a popup, whose
    /// lines never wrap.
    fn pane_text(
        &self,
        pane: TextPane,
    ) -> Option<(
        &ratatui::text::Text<'static>,
        &crate::render::log_view::RenderedLineIndex,
    )> {
        let service = self.state.current_service()?;
        match pane {
            TextPane::Logs => Some((&service.cached_text, &service.cached_line_index)),
            TextPane::Healthcheck => Some((
                &service.healthcheck_cached_text,
                &service.healthcheck_cached_line_index,
            )),
            TextPane::Help | TextPane::Info(_) => None,
        }
    }

    /// The lines a popup shows, every one it can scroll through.
    fn popup_lines(&self) -> &[ratatui::text::Line<'static>] {
        match &self.hits.popup {
            Some(PopupHit {
                content: PopupContent::Text { lines, .. },
                ..
            }) => lines,
            _ => &[],
        }
    }

    /// How many rows `pane` can scroll through.
    pub(crate) fn total_rows(&self, pane: TextPane) -> usize {
        match self.pane_text(pane) {
            Some((_, index)) => index.total_lines(),
            None if matches!(pane, TextPane::Help | TextPane::Info(_)) => self.popup_lines().len(),
            None => 0,
        }
    }

    /// The key of line `index` of `pane`: a log line's sequence number, which stays with it when
    /// older lines are dropped, or else the index itself.
    fn line_key(&self, pane: TextPane, index: usize) -> u64 {
        let index_key = u64::try_from(index).unwrap_or(u64::MAX);
        let Some(service) = self.state.current_service() else {
            return index_key;
        };
        // Each retained log record is one line of the text; were they ever to differ, indices
        // are the only key left.
        if pane == TextPane::Logs && service.cached_lines.len() == service.cached_text.lines.len() {
            return service
                .cached_lines
                .get(index)
                .map_or(index_key, |(seq, _)| *seq);
        }
        index_key
    }

    /// The index of the line `key` names in `pane`, or, when that line was dropped, `Err` with the
    /// index of the line now in its place.
    fn line_index(&self, pane: TextPane, key: u64) -> Result<usize, usize> {
        let index_key = usize::try_from(key).unwrap_or(usize::MAX);
        let Some(service) = self.state.current_service() else {
            return Ok(index_key);
        };
        if pane == TextPane::Logs && service.cached_lines.len() == service.cached_text.lines.len() {
            return service
                .cached_lines
                .binary_search_by_key(&key, |(seq, _)| *seq);
        }
        Ok(index_key)
    }

    /// The anchor for `pos` in `pane`'s text.
    pub(crate) fn anchor_at(&self, pane: TextPane, pos: TextPos) -> TextAnchor {
        let (line, first_row) = self
            .pane_text(pane)
            .and_then(|(_, index)| index.line_of_row(pos.row))
            .unwrap_or((pos.row, pos.row));
        TextAnchor {
            line: self.line_key(pane, line),
            row_in_line: pos.row - first_row,
            col: pos.col,
        }
    }

    /// Where `anchor` is drawn now in `pane`'s text.
    fn place(&self, pane: TextPane, anchor: TextAnchor) -> TextPos {
        let (line, kept) = match self.line_index(pane, anchor.line) {
            Ok(line) => (line, true),
            Err(line) => (line, false),
        };
        let first_row = self
            .pane_text(pane)
            .map_or(Some(line), |(_, index)| index.line_start(line));
        match first_row {
            Some(first_row) if kept => TextPos {
                row: first_row + anchor.row_in_line,
                col: anchor.col,
            },
            // The line was dropped, so the selection starts or ends where the next one does.
            Some(first_row) => TextPos {
                row: first_row,
                col: 0,
            },
            // Every line after it is gone too, as when the log was cleared.
            None => TextPos {
                row: self.total_rows(pane),
                col: 0,
            },
        }
    }

    /// The selection placed on the rows its pane draws now, in text order, or `None` when its
    /// pane no longer shows the text it was made in.
    fn placed(&self) -> Option<(&Selection, Placed)> {
        let selection = self.mouse.selection.as_ref()?;
        let pane = selection.context.pane;
        if self.text_context(pane).as_ref() != Some(&selection.context) {
            return None;
        }
        let anchor = self.place(pane, selection.anchor);
        let head = self.place(pane, selection.head);
        let (start, end) = if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        };
        Some((selection, Placed { start, end }))
    }

    /// Starts a selection from `anchor` to `head` in `context`'s pane.
    pub(crate) fn select(&mut self, context: TextContext, anchor: TextPos, head: TextPos) {
        let pane = context.pane;
        self.mouse.selection = Some(Selection {
            anchor: self.anchor_at(pane, anchor),
            head: self.anchor_at(pane, head),
            context,
        });
    }

    /// Moves the end of the selection being made to `head`.
    pub(crate) fn extend_selection(&mut self, head: TextPos) {
        let Some(pane) = self.mouse.selection.as_ref().map(|s| s.context.pane) else {
            return;
        };
        let head = self.anchor_at(pane, head);
        if let Some(selection) = &mut self.mouse.selection {
            selection.head = head;
        }
    }

    /// Draws `rows` of the text `context` describes off screen, exactly as the pane draws them,
    /// `width` columns wide, into a buffer whose cells without text hold [`UNWRITTEN`].
    fn draw_rows(&self, context: &TextContext, rows: std::ops::Range<usize>, width: u16) -> Buffer {
        let area = Rect::new(0, 0, width, rows_u16(rows.len()));
        let mut buf = Buffer::filled(area, Cell::new(UNWRITTEN));
        if let Some((text, index)) = self.pane_text(context.pane) {
            crate::render::log_view::content_paragraph(
                text,
                index,
                rows.start,
                rows.len(),
                context.wrap,
            )
            .render(area, &mut buf);
        } else {
            let shown = self.popup_lines().get(rows).unwrap_or_default().to_vec();
            Paragraph::new(shown).render(area, &mut buf);
        }
        buf
    }

    /// The first row of the line that `row` is on, in `context`'s pane.
    fn line_start_row(&self, context: &TextContext, row: usize) -> usize {
        self.pane_text(context.pane)
            .and_then(|(_, index)| index.line_of_row(row))
            .map_or(row, |(_, first_row)| first_row)
    }

    /// The rows of the line that `row` is on, in `context`'s pane.
    fn line_rows(&self, context: &TextContext, row: usize) -> std::ops::RangeInclusive<usize> {
        let Some((_, index)) = self.pane_text(context.pane) else {
            return row..=row;
        };
        let Some((line, first)) = index.line_of_row(row) else {
            return row..=row;
        };
        let next = index
            .line_start(line + 1)
            .unwrap_or_else(|| index.total_lines());
        first..=next.saturating_sub(1).max(first)
    }

    /// The line that `row` starts in `context`'s pane, as the pane draws it.
    fn line_starting_at(
        &self,
        context: &TextContext,
        row: usize,
    ) -> Option<&ratatui::text::Line<'static>> {
        match self.pane_text(context.pane) {
            Some((text, index)) => text.lines.get(index.line_of_row(row)?.0),
            None => self.popup_lines().get(row),
        }
    }

    /// The cells of every row of the line `rows` covers, and before each row the whitespace that
    /// wrapping dropped there.
    ///
    /// Only a wrapped line is drawn off screen, one line at a time; an unwrapped one is laid out
    /// from its graphemes, so a long line never needs a buffer as wide as itself.
    fn line_cells(
        &self,
        context: &TextContext,
        rows: &std::ops::RangeInclusive<usize>,
    ) -> Vec<(String, RowCells)> {
        let Some(line) = self.line_starting_at(context, *rows.start()) else {
            return Vec::new();
        };
        if !context.wrap {
            return vec![(String::new(), unwrapped_cells(line))];
        }
        let buf = self.draw_rows(context, *rows.start()..*rows.end() + 1, context.width);
        let mut cursor = LineCursor::new(drawn_graphemes(line).concat());
        (0..buf.area.height)
            .map(|y| {
                let cells = row_cells(&buf, y);
                let shown = cells
                    .iter()
                    .map(|(_, symbol)| symbol.as_str())
                    .collect::<String>();
                (cursor.advance(&shown), cells)
            })
            .collect()
    }

    /// The selected text, or `None` when nothing is selected.
    ///
    /// Rows of one wrapped line join with the whitespace the wrap dropped between them, and a
    /// line that runs past an unwrapped pane's edge is copied whole.
    pub(crate) fn selected_text(&self) -> Option<String> {
        let (selection, placed) = self.placed()?;
        let context = &selection.context;
        let Placed { start, end } = placed;
        if start == end {
            return None;
        }
        let end_row = end.row.min(self.total_rows(context.pane).checked_sub(1)?);

        let mut text = String::new();
        let mut row = start.row;
        while row <= end_row {
            let rows = self.line_rows(context, row);
            for (offset, (gap, cells)) in self.line_cells(context, &rows).into_iter().enumerate() {
                let at = rows.start() + offset;
                if at < start.row || at > end_row {
                    continue;
                }
                if at > start.row {
                    if at == *rows.start() {
                        text.push('\n');
                    } else {
                        text.push_str(&gap);
                    }
                }
                if let (Some((from, to)), Some(text_end)) =
                    (placed.columns_in_row(at), cells_text_end(&cells))
                {
                    text.push_str(&cells_between(
                        &cells,
                        column(from),
                        column(to).min(text_end),
                    ));
                }
            }
            row = rows.end() + 1;
        }
        (!text.is_empty()).then_some(text)
    }

    /// The word around `pos` in `context`'s pane: the run of non-blank cells it is in, or of
    /// blank ones.
    pub(crate) fn word_at(&self, context: &TextContext, pos: TextPos) -> (TextPos, TextPos) {
        let cells = if context.wrap {
            row_cells(
                &self.draw_rows(context, pos.row..pos.row + 1, context.width),
                0,
            )
        } else {
            self.line_starting_at(context, pos.row)
                .map(unwrapped_cells)
                .unwrap_or_default()
        };
        let blank = |col: usize| {
            cells
                .iter()
                .find(|(at, symbol)| {
                    (*at..*at + ratatui::text::Span::raw(symbol.as_str()).width().max(1))
                        .contains(&col)
                })
                .is_none_or(|(_, symbol)| symbol.trim().is_empty())
        };
        let last = cells
            .last()
            .map_or(0, |(at, symbol)| {
                at + ratatui::text::Span::raw(symbol.as_str()).width()
            })
            .max(usize::from(context.width));
        let col = usize::from(pos.col);
        let kind = blank(col);
        let mut from = col;
        while from > 0 && blank(from - 1) == kind {
            from -= 1;
        }
        let mut to = col;
        while to + 1 < last && blank(to + 1) == kind {
            to += 1;
        }
        let as_col = |col: usize| u16::try_from(col).unwrap_or(END_OF_LINE - 1);
        (
            TextPos {
                row: pos.row,
                col: as_col(from),
            },
            TextPos {
                row: pos.row,
                col: as_col(to),
            },
        )
    }

    /// The whole line at `pos` in `context`'s pane, every row of it when it wraps.
    pub(crate) fn line_at(&self, context: &TextContext, pos: TextPos) -> (TextPos, TextPos) {
        let first = self.line_start_row(context, pos.row);
        let last_row = self.total_rows(context.pane).saturating_sub(1).max(pos.row);
        let mut last = pos.row;
        while last < last_row && self.line_start_row(context, last + 1) != last + 1 {
            last += 1;
        }
        (
            TextPos { row: first, col: 0 },
            TextPos {
                row: last,
                col: END_OF_LINE,
            },
        )
    }

    /// Highlights the selected cells that the frame in `buf` shows, after dropping a selection
    /// whose text the panes no longer show, such as one in another service's logs.
    pub(crate) fn paint_selection(&mut self, buf: &mut Buffer) {
        if self.mouse.selection.is_none() {
            return;
        }
        let Some((selection, placed)) = self.placed() else {
            self.mouse.selection = None;
            self.mouse.dragging = false;
            return;
        };
        let context = selection.context.clone();
        let Some(area) = self.text_area(context.pane) else {
            return;
        };
        // A popup over the pane hides the pane's selection.
        let covered = self
            .hits
            .popup
            .as_ref()
            .filter(|_| matches!(context.pane, TextPane::Logs | TextPane::Healthcheck))
            .map(|popup| popup.area);
        let first = self.first_row(context.pane);
        // Rows end where their text does, as terminals highlight a selection, rather than at the
        // pane's edge.
        let drawn = self.draw_rows(
            &context,
            first..first + usize::from(area.height),
            area.width,
        );
        for y in 0..area.height {
            let (Some((from, to)), Some(text_end)) = (
                placed.columns_in_row(first + usize::from(y)),
                text_end(&drawn, y),
            ) else {
                continue;
            };
            for x in from..=to.min(text_end) {
                let position = Position::new(area.x + x, area.y + y);
                if covered.is_some_and(|popup| popup.contains(position)) {
                    continue;
                }
                if let Some(cell) = buf.cell_mut(position) {
                    cell.modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }
}

/// Walks the rows of one drawn line, finding the whitespace that wrapping dropped before each.
///
/// Word wrapping breaks a line at whitespace and draws none of it, so each row's text is found in
/// the line in order, and what lies between two rows is the gap.
#[derive(Debug, Default)]
struct LineCursor {
    line: String,
    at: usize,
}

impl LineCursor {
    fn new(line: String) -> Self {
        Self { line, at: 0 }
    }

    /// Moves past the next row, which shows `row`, and returns what was dropped before it.
    fn advance(&mut self, row: &str) -> String {
        let rest = self.line.get(self.at..).unwrap_or_default();
        let Some(found) = rest.find(row) else {
            return String::new();
        };
        let gap = rest.get(..found).unwrap_or_default().to_string();
        self.at += found + row.len();
        gap
    }
}
