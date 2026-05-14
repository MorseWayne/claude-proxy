#![allow(dead_code)]
// Dracula-inspired color palette
use ratatui::style::{Color, Modifier, Style};

pub const BG: Color = Color::Rgb(30, 30, 34);
pub const BG_DARK: Color = Color::Rgb(22, 22, 26);
pub const BG_SURFACE: Color = Color::Rgb(40, 42, 54);
pub const FG: Color = Color::Rgb(248, 248, 242);
pub const FG_DIM: Color = Color::Rgb(98, 114, 164);
pub const ACCENT: Color = Color::Rgb(139, 233, 253);
pub const ACCENT2: Color = Color::Rgb(80, 250, 123);
pub const WARN: Color = Color::Rgb(241, 250, 140);
pub const ERR: Color = Color::Rgb(255, 85, 85);
pub const BORDER: Color = Color::Rgb(68, 71, 90);

// Footer two-tone palette (inspired by cc-switch-cli)
pub const FOOTER_NAV_BG: Color = Color::Rgb(101, 113, 160);
pub const FOOTER_NAV_FG: Color = Color::Rgb(255, 255, 255);
pub const FOOTER_ACT_BG: Color = Color::Rgb(248, 248, 248);
pub const FOOTER_ACT_FG: Color = Color::Rgb(108, 108, 108);

// Nav highlight symbol
pub const HIGHLIGHT_SYMBOL: &str = " ▸ ";

pub fn selection_style() -> Style {
    Style::default()
        .fg(BG_DARK)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD)
}

pub fn pane_border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(FG_DIM)
    }
}
