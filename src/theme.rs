use ratatui::style::Color;

/// Semantic colors keep widgets consistent without owning the terminal background.
pub struct Theme {
    pub accent: Color,
    pub cyan: Color,
    pub green: Color,
    pub yellow: Color,
    pub red: Color,
    pub purple: Color,
    pub muted: Color,
    pub selection: Color,
    pub cursor_line: Color,
}

/// The initial dark-terminal palette uses accessible, distinct semantic hues.
pub const THEME: Theme = Theme {
    accent: Color::Rgb(94, 173, 255),
    cyan: Color::Rgb(86, 182, 194),
    green: Color::Rgb(152, 195, 121),
    yellow: Color::Rgb(229, 192, 123),
    red: Color::Rgb(224, 108, 117),
    purple: Color::Rgb(198, 120, 221),
    muted: Color::Rgb(130, 140, 153),
    selection: Color::Rgb(42, 52, 64),
    cursor_line: Color::Rgb(30, 34, 42),
};
