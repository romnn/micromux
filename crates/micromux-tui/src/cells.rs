//! Text measured in the cells a terminal draws it in, as ratatui lays it out.
//!
//! Unicode width, which `Line::width` and `Span::width` report, differs from what ratatui draws
//! for some text, such as a halfwidth katakana sound mark, which takes a cell of its own, so
//! anything that places or cuts text on screen measures it here instead.

use ratatui::buffer::CellWidth as _;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation as _;

/// The cells `grapheme` takes; a control character takes none, as ratatui leaves it out.
pub(crate) fn grapheme_cells(grapheme: &str) -> usize {
    if grapheme.contains(char::is_control) {
        0
    } else {
        usize::from(grapheme.cell_width())
    }
}

/// The cells `text` takes.
pub(crate) fn str_cells(text: &str) -> usize {
    text.graphemes(true).map(grapheme_cells).sum()
}

/// The cells `line` takes.
pub(crate) fn line_cells(line: &Line<'_>) -> usize {
    line.spans.iter().map(|span| str_cells(&span.content)).sum()
}

/// Which end of a text a cut keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Keep {
    Start,
    End,
}

/// The longest start or end of `text`, as `keep` says, that fits in `cells` cells, cut between
/// whole graphemes.
pub(crate) fn take_cells(text: &str, cells: usize, keep: Keep) -> &str {
    let mut used = 0;
    match keep {
        Keep::Start => {
            let mut end = 0;
            for (at, grapheme) in text.grapheme_indices(true) {
                used += grapheme_cells(grapheme);
                if used > cells {
                    break;
                }
                end = at + grapheme.len();
            }
            text.get(..end).unwrap_or_default()
        }
        Keep::End => {
            let mut start = text.len();
            for (at, grapheme) in text.grapheme_indices(true).rev() {
                used += grapheme_cells(grapheme);
                if used > cells {
                    break;
                }
                start = at;
            }
            text.get(start..).unwrap_or_default()
        }
    }
}

/// The longest start or end of `line`, as `keep` says, that fits in `cells` cells, cut between
/// whole graphemes, each span keeping its style.
pub(crate) fn fit_line(line: &Line<'_>, cells: usize, keep: Keep) -> Line<'static> {
    let mut room = cells;
    let mut spans = Vec::new();
    let ordered: Box<dyn Iterator<Item = &Span<'_>>> = match keep {
        Keep::Start => Box::new(line.spans.iter()),
        Keep::End => Box::new(line.spans.iter().rev()),
    };
    for span in ordered {
        let kept = take_cells(&span.content, room, keep);
        room -= str_cells(kept);
        if !kept.is_empty() {
            spans.push(Span::styled(kept.to_string(), span.style));
        }
        // A span cut short leaves no room for the spans past it.
        if kept.len() < span.content.len() {
            break;
        }
    }
    if keep == Keep::End {
        spans.reverse();
    }
    Line {
        spans,
        style: line.style,
        alignment: line.alignment,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::text::{Line, Span};
    use similar_asserts::assert_eq;

    use super::{Keep, fit_line, str_cells, take_cells};

    /// Text is measured in the cells ratatui draws it in: a wide character takes two, a
    /// halfwidth katakana with its sound mark three, and a control character none.
    #[test]
    fn text_is_measured_in_cells() {
        assert_eq!(str_cells("ab"), 2);
        assert_eq!(str_cells("界"), 2);
        assert_eq!(str_cells("ｶﾞ"), 3);
        assert_eq!(str_cells("a\tb"), 2);
    }

    /// A cut keeps whole graphemes from either end within the cells it has.
    #[test]
    fn cuts_keep_whole_graphemes() {
        assert_eq!(take_cells("界界界", 5, Keep::Start), "界界");
        assert_eq!(take_cells("abｶﾞ", 4, Keep::Start), "ab");
        assert_eq!(take_cells("abｶﾞ", 4, Keep::End), "bｶﾞ");
        assert_eq!(take_cells("abｶﾞ", 2, Keep::End), "");
        assert_eq!(take_cells("abc", 9, Keep::End), "abc");

        let line = Line::from(vec![Span::raw("ab"), Span::raw("界c")]);
        let texts = |line: Line<'_>| {
            line.spans
                .iter()
                .map(|span| span.content.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(fit_line(&line, 3, Keep::Start)), ["ab"]);
        assert_eq!(texts(fit_line(&line, 4, Keep::Start)), ["ab", "界"]);
        assert_eq!(texts(fit_line(&line, 3, Keep::End)), ["界c"]);
        assert_eq!(texts(fit_line(&line, 4, Keep::End)), ["b", "界c"]);
    }
}
