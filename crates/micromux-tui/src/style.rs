use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style, palette::tailwind},
};

pub const INITIAL_SIDEBAR_WIDTH: u16 = 40;
pub const MIN_SIDEBAR_WIDTH: u16 = 20;

#[must_use]
pub fn health_style(health: Option<micromux::Health>) -> Style {
    match health {
        Some(micromux::Health::Unhealthy) => Style::default().fg(tailwind::RED.c500),
        Some(micromux::Health::Healthy) => Style::default().fg(tailwind::GREEN.c500),
        Some(micromux::Health::Unknown) => Style::default().fg(tailwind::AMBER.c500),
        None => Style::default().fg(tailwind::GREEN.c300),
    }
}

/// Whether a service's panes show output nothing is producing any more.
///
/// A disabled or retired service is stopped, so its logs are a record rather than a live view.
#[must_use]
pub fn is_frozen(snapshot: &micromux::ServiceSnapshot) -> bool {
    snapshot.retired.is_some() || snapshot.desired == micromux::Desired::Disabled
}

#[must_use]
pub fn service_style(snapshot: &micromux::ServiceSnapshot) -> Style {
    if is_frozen(snapshot) {
        return Style::default().fg(FROZEN_TEXT);
    }

    match snapshot.execution {
        // A blocked service is waiting to start, not failing — it shares the pre-start blue rather
        // than the red an `Exited` snapshot would otherwise have given it.
        micromux::Execution::Pending | micromux::Execution::Blocked => {
            Style::default().fg(tailwind::BLUE.c500)
        }
        micromux::Execution::Starting | micromux::Execution::Running => {
            health_style(snapshot.health)
        }
        // Distinct from the green "running" styling so a stopped service is obvious at a glance.
        micromux::Execution::Stopping | micromux::Execution::Unknown => {
            Style::default().fg(tailwind::AMBER.c500)
        }
        micromux::Execution::Exited => Style::default().fg(tailwind::RED.c400),
    }
}

/// The gray of a frozen service's row and of its text that has no color of its own.
const FROZEN_TEXT: Color = tailwind::GRAY.c500;

/// The darkest gray a color turns into when frozen, kept visible on a dark background.
const FROZEN_DARKEST: u8 = 64;

/// How far the brightest color rises above [`FROZEN_DARKEST`], in sixteen steps of this size.
const FROZEN_STEP: u8 = 8;

/// Renders `area` the way a stopped service looks: every color becomes a gray of its own
/// brightness, so bars and highlights keep their shape while nothing looks live.
///
/// Bold, underline, reverse and the other modifiers stay as they are.
/// Text without a color of its own takes the gray of a frozen sidebar row, and a cell without
/// its own background keeps the terminal's.
pub fn freeze(buf: &mut Buffer, area: Rect) {
    for position in area.positions() {
        if let Some(cell) = buf.cell_mut(position) {
            cell.fg = frozen(cell.fg, FROZEN_TEXT);
            cell.bg = frozen(cell.bg, Color::Reset);
        }
    }
}

/// The gray `color` freezes to, or `default` for the terminal's default color.
fn frozen(color: Color, default: Color) -> Color {
    let Some(luma) = luma(color) else {
        return default;
    };
    // The top four bits of the luma pick one of sixteen grays, which keeps the arithmetic in `u8`
    // and still separates the colors a bar or a highlight is made of.
    let step = u8::try_from(luma >> 12).unwrap_or(15);
    let level = FROZEN_DARKEST + step * FROZEN_STEP;
    Color::Rgb(level, level, level)
}

/// The brightness of `color` on a scale of 0 to 65280, or `None` for the terminal's default.
///
/// Named and indexed colors take xterm's palette, which most terminals ship as their default.
fn luma(color: Color) -> Option<u16> {
    let (red, green, blue) = match color {
        Color::Reset => return None,
        Color::Black | Color::Indexed(0) => (0, 0, 0),
        Color::Red | Color::Indexed(1) => (205, 0, 0),
        Color::Green | Color::Indexed(2) => (0, 205, 0),
        Color::Yellow | Color::Indexed(3) => (205, 205, 0),
        Color::Blue | Color::Indexed(4) => (0, 0, 238),
        Color::Magenta | Color::Indexed(5) => (205, 0, 205),
        Color::Cyan | Color::Indexed(6) => (0, 205, 205),
        Color::Gray | Color::Indexed(7) => (229, 229, 229),
        Color::DarkGray | Color::Indexed(8) => (127, 127, 127),
        Color::LightRed | Color::Indexed(9) => (255, 0, 0),
        Color::LightGreen | Color::Indexed(10) => (0, 255, 0),
        Color::LightYellow | Color::Indexed(11) => (255, 255, 0),
        Color::LightBlue | Color::Indexed(12) => (92, 92, 255),
        Color::LightMagenta | Color::Indexed(13) => (255, 0, 255),
        Color::LightCyan | Color::Indexed(14) => (0, 255, 255),
        Color::White | Color::Indexed(15) => (255, 255, 255),
        Color::Rgb(red, green, blue) => (red, green, blue),
        // The 6x6x6 color cube, whose non-zero levels start at 95 and rise by 40.
        Color::Indexed(index @ 16..=231) => {
            let level = |steps: u8| if steps == 0 { 0 } else { 55 + 40 * steps };
            let index = index - 16;
            (level(index / 36), level(index / 6 % 6), level(index % 6))
        }
        // The grayscale ramp from 8 to 238 in steps of 10.
        Color::Indexed(index @ 232..=255) => {
            let level = 8 + 10 * (index - 232);
            (level, level, level)
        }
    };
    // Rec. 601 luma with weights that sum to 256, so the result fits `u16` without a division.
    Some(77 * u16::from(red) + 151 * u16::from(green) + 28 * u16::from(blue))
}

#[cfg(test)]
mod tests {
    use super::{freeze, frozen, luma};
    use color_eyre::eyre::{self, OptionExt as _};
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier, Style},
        text::{Line, Span},
    };
    use similar_asserts::assert_eq;

    /// Every color family maps onto xterm's palette, and the terminal's default has no luma.
    #[test]
    fn luma_follows_the_xterm_palette() {
        assert_eq!(luma(Color::Reset), None);
        assert_eq!(luma(Color::Black), Some(0));
        assert_eq!(luma(Color::White), Some(256 * 255));
        // Named colors and their indexed twins agree.
        assert_eq!(luma(Color::Red), luma(Color::Indexed(1)));
        assert_eq!(luma(Color::LightBlue), luma(Color::Indexed(12)));
        // Green outshines red, which outshines blue, as it does on screen.
        assert!(luma(Color::Green) > luma(Color::Red));
        assert!(luma(Color::Red) > luma(Color::Blue));
        // The cube's first entry is black, its last is white, and the ramp climbs by ten.
        assert_eq!(luma(Color::Indexed(16)), Some(0));
        assert_eq!(luma(Color::Indexed(231)), luma(Color::White));
        assert_eq!(luma(Color::Indexed(232)), luma(Color::Rgb(8, 8, 8)));
        assert_eq!(luma(Color::Indexed(255)), luma(Color::Rgb(238, 238, 238)));
    }

    /// The gray level `color` freezes to.
    fn level(color: Color) -> eyre::Result<u8> {
        match frozen(color, Color::Reset) {
            Color::Rgb(red, green, blue) if red == green && green == blue => Ok(red),
            other => eyre::bail!("expected a gray, got {other:?}"),
        }
    }

    /// Frozen grays keep the order of the colors' brightness inside a visible range.
    #[test]
    fn frozen_grays_keep_brightness_order() -> eyre::Result<()> {
        assert_eq!(level(Color::Black)?, 64);
        assert_eq!(level(Color::White)?, 184);
        assert!(level(Color::Yellow)? > level(Color::Green)?);
        assert!(level(Color::Green)? > level(Color::Red)?);
        assert!(level(Color::Red)? > level(Color::Blue)?);
        assert!(level(Color::Blue)? > level(Color::Black)?);
        // The default color takes whatever the caller stands in for it.
        assert_eq!(frozen(Color::Reset, Color::Reset), Color::Reset);
        assert_eq!(
            frozen(Color::Reset, Color::Rgb(1, 2, 3)),
            Color::Rgb(1, 2, 3)
        );
        Ok(())
    }

    /// Freezing an area grays its colors, keeps its modifiers, and leaves the rest of the buffer
    /// untouched.
    #[test]
    fn freeze_grays_colors_and_keeps_modifiers() -> eyre::Result<()> {
        let area = Rect::new(0, 0, 6, 2);
        let mut buf = Buffer::empty(area);
        let bold_red = Style::default()
            .fg(Color::Red)
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD);
        buf.set_line(0, 0, &Line::from(Span::styled("bar", bold_red)), 6);
        buf.set_line(0, 1, &Line::from(Span::styled("keep", bold_red)), 6);

        freeze(&mut buf, Rect::new(0, 0, 6, 1));

        let frozen_cell = buf.cell((0, 0)).ok_or_eyre("missing frozen cell")?;
        assert!(matches!(frozen_cell.fg, Color::Rgb(level, _, _) if level > 64));
        assert!(matches!(frozen_cell.bg, Color::Rgb(..)));
        assert_ne!(frozen_cell.fg, frozen_cell.bg);
        assert!(frozen_cell.modifier.contains(Modifier::BOLD));
        // An empty cell has no color of its own: its text goes gray and its background stays.
        let empty_cell = buf.cell((5, 0)).ok_or_eyre("missing empty cell")?;
        assert_eq!(empty_cell.fg, super::FROZEN_TEXT);
        assert_eq!(empty_cell.bg, Color::Reset);
        // The row outside the area keeps its colors.
        let kept_cell = buf.cell((0, 1)).ok_or_eyre("missing kept cell")?;
        assert_eq!(kept_cell.fg, Color::Red);
        assert_eq!(kept_cell.bg, Color::Blue);
        Ok(())
    }
}
