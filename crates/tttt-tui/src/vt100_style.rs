use ratatui::style::{Color, Modifier, Style};

/// Convert a vt100::Cell's attributes into a ratatui Style.
pub fn cell_style(cell: &vt100::Cell) -> Style {
    let fg = convert_color(cell.fgcolor());
    let bg = convert_color(cell.bgcolor());

    let mut style = Style::default();
    if let Some(c) = fg {
        style = style.fg(c);
    }
    if let Some(c) = bg {
        style = style.bg(c);
    }
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }

    style
}

/// Convert a vt100::Color into an optional ratatui Color.
pub fn convert_color(color: vt100::Color) -> Option<Color> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_parser_with(input: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(2, 10, 0);
        parser.process(input);
        parser
    }

    #[test]
    fn test_default_color_returns_none() {
        assert_eq!(convert_color(vt100::Color::Default), None);
    }

    #[test]
    fn test_indexed_color_converts() {
        assert_eq!(convert_color(vt100::Color::Idx(5)), Some(Color::Indexed(5)));
    }

    #[test]
    fn test_rgb_color_converts() {
        assert_eq!(
            convert_color(vt100::Color::Rgb(255, 128, 0)),
            Some(Color::Rgb(255, 128, 0))
        );
    }

    #[test]
    fn test_cell_style_default() {
        // A plain character with no attributes should yield Style::default()
        let parser = make_parser_with(b"X");
        let cell = parser.screen().cell(0, 0).unwrap();
        assert_eq!(cell_style(cell), Style::default());
    }

    #[test]
    fn test_cell_style_bold() {
        // ESC[1m sets bold
        let parser = make_parser_with(b"\x1b[1mX");
        let cell = parser.screen().cell(0, 0).unwrap();
        let style = cell_style(cell);
        assert!(style.add_modifier == Modifier::BOLD || style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn test_cell_style_multiple_attrs() {
        // ESC[1;3;4m = bold + italic + underline
        let parser = make_parser_with(b"\x1b[1;3;4mX");
        let cell = parser.screen().cell(0, 0).unwrap();
        let style = cell_style(cell);
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(style.add_modifier.contains(Modifier::ITALIC));
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn test_cell_style_with_colors() {
        // ESC[31m = red fg (indexed color 1), ESC[42m = green bg (indexed color 2)
        let parser = make_parser_with(b"\x1b[31;42mX");
        let cell = parser.screen().cell(0, 0).unwrap();
        let style = cell_style(cell);
        assert_eq!(style.fg, Some(Color::Indexed(1)));
        assert_eq!(style.bg, Some(Color::Indexed(2)));
    }

    #[test]
    fn test_cell_style_inverse() {
        // ESC[7m = reverse video
        let parser = make_parser_with(b"\x1b[7mX");
        let cell = parser.screen().cell(0, 0).unwrap();
        let style = cell_style(cell);
        assert!(style.add_modifier.contains(Modifier::REVERSED));
    }
}
