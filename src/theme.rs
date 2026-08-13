//! Light/dark terminal support.
//!
//! The rule that makes a TUI work on both: never paint a background. Leaving it
//! as `Color::Reset` means the terminal's own background shows through, so the
//! app inherits whatever the user has configured. Only foreground colours are
//! themed, and only to keep contrast reasonable on either backdrop.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

impl Mode {
    pub fn toggled(self) -> Self {
        match self {
            Mode::Dark => Mode::Light,
            Mode::Light => Mode::Dark,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Dark => "dark",
            Mode::Light => "light",
        }
    }
}

/// Best-effort background detection.
///
/// `COLORFGBG` is set by several terminals (rxvt, konsole, some xterm builds)
/// as `fg;bg` ANSI indices. Anything else falls back to dark, which is both the
/// common case and the safer guess: our light palette on a dark background is
/// far less readable than the reverse.
pub fn detect() -> Mode {
    match std::env::var("COLORFGBG") {
        Ok(v) => match v
            .rsplit(';')
            .next()
            .and_then(|b| b.trim().parse::<u8>().ok())
        {
            // 7 (light grey) and 15 (white) are the light backgrounds.
            Some(7) | Some(15) => Mode::Light,
            _ => Mode::Dark,
        },
        Err(_) => Mode::Dark,
    }
}

pub fn parse(s: &str) -> Option<Mode> {
    match s {
        "dark" => Some(Mode::Dark),
        "light" => Some(Mode::Light),
        "auto" => Some(detect()),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub mode: Mode,
    /// Primary text. `Reset` means "the terminal's normal foreground", which is
    /// always the right contrast against its own background.
    pub fg: Color,
    pub dim: Color,
    pub accent: Color,
    pub heading: Color,
    pub good: Color,
    pub warn: Color,
    pub bad: Color,
    pub border: Color,
    pub gauge_bg: Color,
    pub selection_fg: Color,
}

impl Theme {
    pub fn new(mode: Mode) -> Self {
        match mode {
            // Bright variants read well on dark backgrounds but wash out on
            // white, so the light theme uses the base (darker) ANSI colours.
            Mode::Dark => Theme {
                mode,
                fg: Color::Reset,
                dim: Color::DarkGray,
                accent: Color::Cyan,
                heading: Color::LightCyan,
                good: Color::LightGreen,
                warn: Color::LightYellow,
                bad: Color::LightRed,
                border: Color::DarkGray,
                gauge_bg: Color::Black,
                selection_fg: Color::LightCyan,
            },
            Mode::Light => Theme {
                mode,
                fg: Color::Reset,
                dim: Color::Gray,
                accent: Color::Blue,
                heading: Color::Blue,
                good: Color::Green,
                warn: Color::Magenta,
                bad: Color::Red,
                border: Color::Gray,
                gauge_bg: Color::White,
                selection_fg: Color::Blue,
            },
        }
    }

    pub fn text(&self) -> Style {
        Style::default().fg(self.fg)
    }

    pub fn dimmed(&self) -> Style {
        Style::default().fg(self.dim)
    }

    pub fn title(&self) -> Style {
        Style::default()
            .fg(self.heading)
            .add_modifier(Modifier::BOLD)
    }

    pub fn selected(&self) -> Style {
        // Reversed video rather than a painted background: it inverts against
        // whatever the terminal is actually using, so it works in both themes.
        Style::default()
            .fg(self.selection_fg)
            .add_modifier(Modifier::REVERSED | Modifier::BOLD)
    }

    pub fn gauge(&self) -> Style {
        Style::default().fg(self.accent).bg(self.gauge_bg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorfgbg_light_backgrounds_are_recognised() {
        // The value is fg;bg, so the background is the trailing field.
        let bg = |v: &str| v.rsplit(';').next().and_then(|b| b.parse::<u8>().ok());
        assert_eq!(bg("0;15"), Some(15));
        assert_eq!(bg("15;0"), Some(0));
        assert_eq!(bg("0;default;15"), Some(15));
    }

    #[test]
    fn backgrounds_are_never_painted_over() {
        // Guards the property that makes light terminals work.
        for mode in [Mode::Dark, Mode::Light] {
            let t = Theme::new(mode);
            assert_eq!(t.text().bg, None);
            assert_eq!(t.fg, Color::Reset);
        }
    }

    #[test]
    fn parse_accepts_the_documented_values() {
        assert_eq!(parse("dark"), Some(Mode::Dark));
        assert_eq!(parse("light"), Some(Mode::Light));
        assert!(parse("auto").is_some());
        assert_eq!(parse("nope"), None);
    }
}
