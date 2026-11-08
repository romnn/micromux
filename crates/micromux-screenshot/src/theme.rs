//! The terminal themes the screenshots are drawn in.

/// A terminal theme the screenshots are drawn in.
///
/// The first sixteen colors come from the terminal's theme, as they do in a real terminal, so
/// micromux's own palette colors and those of the service output render as a user sees them
/// instead of in `freeze`'s much darker defaults.
pub(crate) struct Theme {
    /// Appended to the scenario's name to name the image.
    pub(crate) suffix: &'static str,
    pub(crate) palette: [Rgb; 16],
    pub(crate) foreground: Rgb,
    /// The window's fill and the default background.
    pub(crate) background: Rgb,
}

pub(crate) type Rgb = (u8, u8, u8);

/// Ghostty's "GitHub Dark High Contrast" for GitHub's dark mode and "Catppuccin Latte" for its
/// light mode.
///
/// GitHub's own light themes turn yellow into a dark brown that reads as black, while Latte keeps
/// an amber that still reads as yellow.
pub(crate) const THEMES: [Theme; 2] = [
    Theme {
        suffix: "",
        palette: [
            (0x7a, 0x82, 0x8e),
            (0xff, 0x94, 0x92),
            (0x26, 0xcd, 0x4d),
            (0xf0, 0xb7, 0x2f),
            (0x71, 0xb7, 0xff),
            (0xcb, 0x9e, 0xff),
            (0x39, 0xc5, 0xcf),
            (0xd9, 0xde, 0xe3),
            (0x9e, 0xa7, 0xb3),
            (0xff, 0xb1, 0xaf),
            (0x4a, 0xe1, 0x68),
            (0xf7, 0xc8, 0x43),
            (0x91, 0xcb, 0xff),
            (0xdb, 0xb7, 0xff),
            (0x56, 0xd4, 0xdd),
            (0xff, 0xff, 0xff),
        ],
        foreground: (0xf0, 0xf3, 0xf6),
        background: (0x0a, 0x0c, 0x10),
    },
    Theme {
        suffix: "-light",
        palette: [
            (0x5c, 0x5f, 0x77),
            (0xd2, 0x0f, 0x39),
            (0x40, 0xa0, 0x2b),
            (0xdf, 0x8e, 0x1d),
            (0x1e, 0x66, 0xf5),
            (0xea, 0x76, 0xcb),
            (0x17, 0x92, 0x99),
            (0xac, 0xb0, 0xbe),
            (0x6c, 0x6f, 0x85),
            (0xde, 0x29, 0x3e),
            (0x49, 0xaf, 0x3d),
            (0xee, 0xa0, 0x2d),
            (0x45, 0x6e, 0xff),
            (0xfe, 0x85, 0xd8),
            (0x2d, 0x9f, 0xa8),
            (0xbc, 0xc0, 0xcc),
        ],
        foreground: (0x4c, 0x4f, 0x69),
        background: (0xef, 0xf1, 0xf5),
    },
];
