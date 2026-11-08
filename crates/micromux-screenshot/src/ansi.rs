//! Turning the emulator's screen back into ANSI text for `freeze`, in a theme's colors.

use crate::theme::{Rgb, Theme};
use std::fmt::Write as _;

/// A 24-bit/indexed color, in our own representation so equality (for run coalescing) does not
/// depend on `vt100`'s derives.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

impl From<vt100::Color> for Col {
    fn from(color: vt100::Color) -> Self {
        match color {
            vt100::Color::Default => Self::Default,
            vt100::Color::Idx(i) => Self::Idx(i),
            vt100::Color::Rgb(r, g, b) => Self::Rgb(r, g, b),
        }
    }
}

/// The effective style of a single cell.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Sty {
    fg: Col,
    bg: Col,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

const DEFAULT_STY: Sty = Sty {
    fg: Col::Default,
    bg: Col::Default,
    bold: false,
    dim: false,
    italic: false,
    underline: false,
    inverse: false,
};

/// Serialize the emulator screen to a plain ANSI string, walking every cell and emitting explicit
/// spaces for blank cells.
///
/// We do not use `vt100`'s own `rows_formatted`/`contents_formatted`: those coalesce runs of blank
/// cells into cursor-forward (`CUF`) escapes, which `freeze --language text` does not expand back
/// into spaces, so columns collapse. Emitting real spaces — exactly what a terminal shows — renders
/// faithfully. Each styled run is introduced by a full reset so the bare `\x1b[39m`/`\x1b[49m`
/// codes (which `freeze` ignores) never appear.
pub(crate) fn screen_to_ansi(
    screen: &vt100::Screen,
    rows: u16,
    cols: u16,
    theme: &Theme,
) -> String {
    let mut out = String::new();
    for row in 0..rows {
        let mut current = DEFAULT_STY;
        for col in 0..cols {
            let cell = screen.cell(row, col);
            if cell.is_some_and(vt100::Cell::is_wide_continuation) {
                continue;
            }

            let (text, sty) = match cell {
                Some(cell) => {
                    let text = if cell.has_contents() {
                        cell.contents()
                    } else {
                        " "
                    };
                    (
                        text,
                        Sty {
                            fg: cell.fgcolor().into(),
                            bg: cell.bgcolor().into(),
                            bold: cell.bold(),
                            dim: cell.dim(),
                            italic: cell.italic(),
                            underline: cell.underline(),
                            inverse: cell.inverse(),
                        }
                        .resolve_inverse(theme),
                    )
                }
                None => (" ", DEFAULT_STY),
            };

            if sty != current {
                out.push_str("\x1b[0");
                if sty.bold {
                    out.push_str(";1");
                }
                if sty.dim {
                    out.push_str(";2");
                }
                if sty.italic {
                    out.push_str(";3");
                }
                if sty.underline {
                    out.push_str(";4");
                }
                if sty.inverse {
                    out.push_str(";7");
                }
                push_color(&mut out, sty.fg, false, theme);
                push_color(&mut out, sty.bg, true, theme);
                out.push('m');
                current = sty;
            }
            out.push_str(text);
        }
        if current != DEFAULT_STY {
            out.push_str("\x1b[0m");
        }
        out.push('\n');
    }
    out
}

impl From<Rgb> for Col {
    fn from((r, g, b): Rgb) -> Self {
        Self::Rgb(r, g, b)
    }
}

impl Sty {
    /// Swaps the colors of reverse video, which `freeze` does not draw, such as a mouse
    /// selection.
    fn resolve_inverse(self, theme: &Theme) -> Self {
        if !self.inverse {
            return self;
        }
        let fg = if self.fg == Col::Default {
            theme.foreground.into()
        } else {
            self.fg
        };
        let bg = if self.bg == Col::Default {
            theme.background.into()
        } else {
            self.bg
        };
        Self {
            fg: bg,
            bg: fg,
            inverse: false,
            ..self
        }
    }
}

/// Append the SGR parameters for `color` (each with a leading `;`) in `theme`'s colors.
///
/// A default background appends nothing and shows the window's fill.
fn push_color(out: &mut String, color: Col, background: bool, theme: &Theme) {
    let (extended, indexed) = if background {
        (";48;2;", ";48;5;")
    } else {
        (";38;2;", ";38;5;")
    };
    let color = match color {
        Col::Default if !background => theme.foreground.into(),
        Col::Idx(i) => theme
            .palette
            .get(usize::from(i))
            .map_or(color, |&rgb| rgb.into()),
        other => other,
    };
    match color {
        Col::Default => {}
        Col::Idx(i) => {
            out.push_str(indexed);
            out.push_str(&i.to_string());
        }
        Col::Rgb(r, g, b) => {
            out.push_str(extended);
            let _ = write!(out, "{r};{g};{b}");
        }
    }
}
