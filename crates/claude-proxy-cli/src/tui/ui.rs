use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    widgets::Paragraph,
};

use super::app::{App, Focus, NavItem, Overlay, ProviderFocus};
use super::pages;
use super::theme;
use super::widgets;

pub fn render(f: &mut Frame, app: &App) {
    let area = f.area();

    // Full-screen background
    f.render_widget(
        Paragraph::new("").style(Style::default().bg(theme::BG)),
        area,
    );

    // Layout: header (bordered, 3 lines), body (nav + content), footer (1 line)
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header with border
            Constraint::Min(0),    // body
            Constraint::Length(1), // footer
        ])
        .split(area);

    widgets::render_header(f, root[0], app);

    // Body: nav panel + content
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(22), // wider nav for icon + text table
            Constraint::Min(0),     // content
        ])
        .split(root[1]);

    widgets::render_nav(f, body[0], app);
    render_content(f, body[1], app);

    // Footer key hints (two-tone style)
    let (nav_hints, act_hints) = get_footer_hints(app);
    widgets::render_footer(f, root[2], &nav_hints, &act_hints);

    // Toast (floating, top-right of content area)
    widgets::render_toast(f, body[1], app);

    // Overlay (centered over content)
    if let Some(ref overlay) = app.overlay {
        match overlay {
            Overlay::Confirm(c) => {
                widgets::render_confirm_overlay(f, body[1], &c.title, &c.message, &c.kind)
            }
            Overlay::Input(input) => widgets::render_input_overlay(f, body[1], input),
            Overlay::Picker(picker) => widgets::render_picker_overlay(f, body[1], picker),
            Overlay::Loading(loading) => widgets::render_loading_overlay(f, body[1], loading),
            Overlay::Help => widgets::render_help_overlay(f, body[1]),
        }
    }
}

fn render_content(f: &mut Frame, area: Rect, app: &App) {
    match app.nav {
        NavItem::Dashboard => pages::render_dashboard(f, app, area),
        NavItem::Providers => pages::render_providers(f, app, area),
        NavItem::System => pages::render_system_page(f, app, area),
        _ => pages::render_settings_page(f, app, area),
    }
}

type HintPair = (&'static str, &'static str);

/// Returns (navigation_hints, action_hints) for the two-tone footer.
fn get_footer_hints(app: &App) -> (Vec<HintPair>, Vec<HintPair>) {
    if let Some(overlay) = &app.overlay {
        let acts = match overlay {
            Overlay::Confirm(c) => match c.kind {
                super::app::ConfirmKind::DirtyQuit => vec![
                    ("Enter", "Save & Quit"),
                    ("n", "Discard"),
                    ("Esc", "Cancel"),
                ],
                _ => vec![("Enter", "Yes"), ("Esc", "No")],
            },
            Overlay::Input(_) => vec![("Enter", "Confirm"), ("Esc", "Cancel")],
            Overlay::Picker(_) => vec![("↑↓", "move"), ("Enter", "select"), ("Esc", "cancel")],
            Overlay::Loading(_) => vec![("Esc", "cancel")],
            Overlay::Help => vec![("Esc", "Close")],
        };
        return (vec![], acts);
    }

    let nav = vec![("←→", "menu/content"), ("↑↓", "move")];

    let acts = match app.focus {
        Focus::Nav => vec![
            ("Enter", "select"),
            ("Esc", "back"),
            ("q", "quit"),
            ("?", "help"),
        ],
        Focus::Content => {
            if app.nav == NavItem::Providers {
                match app.provider_focus {
                    ProviderFocus::List => vec![
                        ("→/Enter", "detail"),
                        ("a", "add"),
                        ("d", "delete"),
                        ("Esc", "back"),
                        ("?", "help"),
                    ],
                    ProviderFocus::Detail => vec![
                        ("e/Enter", "edit"),
                        ("←/Esc", "list"),
                        ("Ctrl+S", "save"),
                        ("?", "help"),
                    ],
                }
            } else {
                let mut base = vec![("e", "edit"), ("Ctrl+S", "save"), ("Esc", "back")];
                if app.nav == NavItem::Log {
                    base.push(("Space", "toggle"));
                }
                base.push(("?", "help"));
                base
            }
        }
        Focus::Overlay => vec![],
    };

    (nav, acts)
}
