/// ANSI terminal escape sequence utilities.

/// Move cursor to (row, col), 1-indexed.
pub fn cursor_goto(row: u16, col: u16) -> String {
    format!("\x1b[{};{}H", row, col)
}

/// Clear from cursor to end of line.
pub fn clear_to_eol() -> &'static str {
    "\x1b[K"
}

/// Clear the entire screen and move cursor to top-left.
pub fn clear_screen() -> &'static str {
    "\x1b[2J\x1b[1;1H"
}

/// Terminal text attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attribute {
    Reset,
    Bold,
    Dim,
    /// Foreground color (30-37, 90-97) on background color (40-47, 100-107).
    Colors { fg: u8, bg: u8 },
    /// Just foreground.
    Fg(u8),
}

/// Generate the ANSI escape for a text attribute.
pub fn set_attribute(attr: Attribute) -> String {
    match attr {
        Attribute::Reset => "\x1b[0m".to_string(),
        Attribute::Bold => "\x1b[1m".to_string(),
        Attribute::Dim => "\x1b[2m".to_string(),
        Attribute::Colors { fg, bg } => format!("\x1b[{};{}m", fg, bg),
        Attribute::Fg(fg) => format!("\x1b[{}m", fg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cursor_goto() {
        assert_eq!(cursor_goto(1, 1), "\x1b[1;1H");
        assert_eq!(cursor_goto(10, 50), "\x1b[10;50H");
    }

    #[test]
    fn test_clear_to_eol() {
        assert_eq!(clear_to_eol(), "\x1b[K");
    }

    #[test]
    fn test_clear_screen() {
        let s = clear_screen();
        assert!(s.contains("\x1b[2J"));
        assert!(s.contains("\x1b[1;1H"));
    }

    #[test]
    fn test_attribute_reset() {
        assert_eq!(set_attribute(Attribute::Reset), "\x1b[0m");
    }

    #[test]
    fn test_attribute_bold() {
        assert_eq!(set_attribute(Attribute::Bold), "\x1b[1m");
    }

    #[test]
    fn test_attribute_colors() {
        // Black on white
        assert_eq!(
            set_attribute(Attribute::Colors { fg: 30, bg: 47 }),
            "\x1b[30;47m"
        );
    }

    #[test]
    fn test_attribute_fg() {
        assert_eq!(set_attribute(Attribute::Fg(32)), "\x1b[32m");
    }
}
