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

/// Everything drawn on row `y` of `buf`, from column `from` to `to`, both included.
fn drawn_between(buf: &Buffer, y: u16, from: u16, to: u16) -> String {
    (from..=to.min(buf.area.width.saturating_sub(1)))
        .filter_map(|x| buf.cell((x, y)).map(Cell::symbol))
        .filter(|symbol| *symbol != UNWRITTEN)
        .collect()
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
        let last_row = self.total_rows(context.pane).checked_sub(1)?;
        let end_row = end.row.min(last_row);
        // Draw from the start of the first line, so the whitespace dropped where it wraps can be
        // found, and wide enough for the longest line when lines run past the pane's edge.
        let first = self.line_start_row(context, start.row);
        let width = match self.pane_text(context.pane) {
            Some((text, index)) if !context.wrap => (first..=end_row)
                .filter_map(|row| index.line_of_row(row))
                .filter_map(|(line, _)| text.lines.get(line).map(ratatui::text::Line::width))
                .max()
                .map_or(context.width, |widest| rows_u16(widest).max(context.width)),
            _ => context.width,
        };
        let buf = self.draw_rows(context, first..end_row + 1, width);

        let mut text = String::new();
        let mut line = LineCursor::default();
        for row in first..=end_row {
            let y = rows_u16(row - first);
            let starts_line = self.line_start_row(context, row) == row;
            if starts_line {
                line = LineCursor::new(self.drawn_line(context, row));
            }
            let gap = line.advance(&drawn_between(&buf, y, 0, END_OF_LINE));
            if row < start.row {
                continue;
            }
            if row > start.row {
                if starts_line {
                    text.push('\n');
                } else {
                    text.push_str(&gap);
                }
            }
            let (Some((from, to)), Some(text_end)) =
                (placed.columns_in_row(row), text_end(&buf, y))
            else {
                continue;
            };
            text.push_str(&drawn_between(&buf, y, from, to.min(text_end)));
        }
        (!text.is_empty()).then_some(text)
    }

    /// The text of the line starting on `row` as the pane draws it, without the control and
    /// zero-width characters that never land in a cell.
    fn drawn_line(&self, context: &TextContext, row: usize) -> String {
        let line = self
            .pane_text(context.pane)
            .and_then(|(text, index)| text.lines.get(index.line_of_row(row)?.0));
        line.map(|line| {
            line.styled_graphemes(ratatui::style::Style::default())
                .map(|grapheme| grapheme.symbol)
                .filter(|symbol| {
                    !symbol.contains(char::is_control)
                        && ratatui::text::Span::raw(*symbol).width() > 0
                })
                .collect()
        })
        .unwrap_or_default()
    }

    /// The word around `pos` in `context`'s pane: the run of non-blank cells it is in, or of
    /// blank ones.
    pub(crate) fn word_at(&self, context: &TextContext, pos: TextPos) -> (TextPos, TextPos) {
        let buf = self.draw_rows(context, pos.row..pos.row + 1, context.width);
        let blank = |x: u16| {
            buf.cell((x, 0))
                .is_none_or(|cell| cell.symbol() == UNWRITTEN || cell.symbol().trim().is_empty())
        };
        let kind = blank(pos.col);
        let mut from = pos.col;
        while from > 0 && blank(from - 1) == kind {
            from -= 1;
        }
        let mut to = pos.col;
        while to + 1 < context.width && blank(to + 1) == kind {
            to += 1;
        }
        // A word that reaches the pane's edge of an unwrapped pane goes on past it.
        if !context.wrap && to + 1 == context.width && !kind {
            to = END_OF_LINE;
        }
        (
            TextPos {
                row: pos.row,
                col: from,
            },
            TextPos {
                row: pos.row,
                col: to,
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
