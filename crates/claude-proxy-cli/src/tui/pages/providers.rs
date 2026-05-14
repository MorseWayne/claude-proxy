use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, Paragraph},
};

use super::super::app::{App, Focus, ProviderFocus};
use super::super::{theme, widgets};

pub fn render_providers(f: &mut Frame, app: &App, area: Rect) {
    if app.settings.providers.is_empty() {
        render_empty_state(f, app, area);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);

    render_provider_list(f, app, chunks[0]);
    render_provider_detail(f, app, chunks[1]);
}

fn render_empty_state(f: &mut Frame, app: &App, area: Rect) {
    let inner = widgets::render_content_frame(f, area, app, "Providers");

    let title_style = Style::default().fg(theme::FG).add_modifier(Modifier::BOLD);
    let subtitle_style = Style::default().fg(theme::FG_DIM);
    let action_style = Style::default()
        .fg(theme::BG_DARK)
        .bg(theme::ACCENT)
        .add_modifier(Modifier::BOLD);

    let text = vec![
        Line::from(""),
        Line::from(Span::styled("  No providers configured", title_style)),
        Line::from(""),
        Line::from(Span::styled(
            "  Add a provider to get started.",
            subtitle_style,
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(" a ", action_style),
            Span::styled("  Add provider", Style::default().fg(theme::FG)),
        ]),
    ];
    f.render_widget(Paragraph::new(text), inner);
}

fn render_provider_list(f: &mut Frame, app: &App, area: Rect) {
    let is_focused =
        matches!(app.focus, Focus::Content) && matches!(app.provider_focus, ProviderFocus::List);

    let block = Block::default()
        .title(" Providers ")
        .title_style(if is_focused {
            Style::default().fg(theme::FG).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::FG_DIM)
        })
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::pane_border_style(is_focused))
        .style(Style::default().bg(theme::BG_DARK));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let items: Vec<ListItem> = app
        .settings
        .providers
        .keys()
        .map(|id| {
            let is_default = app.settings.model.default.starts_with(&format!("{id}/"));
            let marker = if is_default { " ★" } else { "" };
            let line = Line::from(vec![
                Span::styled(format!("  {id}"), Style::default().fg(theme::FG)),
                Span::styled(marker, Style::default().fg(theme::WARN)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let highlight = if is_focused {
        widgets::selection_style()
    } else {
        Style::default().fg(theme::FG_DIM).bg(theme::BG_SURFACE)
    };

    let idx = app.content_idx.min(items.len().saturating_sub(1));
    f.render_stateful_widget(
        List::new(items).highlight_style(highlight),
        inner,
        &mut ratatui::widgets::ListState::default().with_selected(Some(idx)),
    );
}

fn render_provider_detail(f: &mut Frame, app: &App, area: Rect) {
    let detail_focused =
        matches!(app.focus, Focus::Content) && matches!(app.provider_focus, ProviderFocus::Detail);

    // Use custom block for detail pane with its own focus styling
    let block = Block::default()
        .title(" Detail ")
        .title_style(if detail_focused {
            Style::default().fg(theme::FG).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::FG_DIM)
        })
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::pane_border_style(detail_focused))
        .style(Style::default().bg(theme::BG));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.settings.providers.is_empty() {
        return;
    }

    let idx = app
        .content_idx
        .min(app.settings.providers.len().saturating_sub(1));
    let (id, cfg) = app.settings.providers.iter().nth(idx).unwrap();
    let is_copilot = id == "copilot";

    let rows = widgets::field_rows(inner, 7);

    // API Key (detail_idx == 0)
    let key_display = if is_copilot {
        "OAuth (auto)"
    } else {
        &cfg.api_key
    };
    widgets::render_field(
        f,
        rows[0],
        "API Key",
        key_display,
        detail_focused && app.detail_idx == 0,
        !is_copilot,
    );

    // Base URL (detail_idx == 1)
    widgets::render_field(
        f,
        rows[1],
        "Base URL",
        &cfg.base_url,
        detail_focused && app.detail_idx == 1,
        false,
    );

    // Proxy (detail_idx == 2)
    let proxy_display = if cfg.proxy.is_empty() {
        "(none)"
    } else {
        &cfg.proxy
    };
    widgets::render_field(
        f,
        rows[2],
        "Proxy",
        proxy_display,
        detail_focused && app.detail_idx == 2,
        false,
    );

    // Status (read-only)
    let is_default = app.settings.model.default.starts_with(&format!("{id}/"));
    let status = if is_default {
        format!("Default ({})", app.settings.model.default)
    } else {
        "Not default".into()
    };
    widgets::render_field(f, rows[3], "Status", &status, false, false);

    // Copilot info (read-only)
    if is_copilot && let Some(ref cc) = cfg.copilot {
        let info = format!(
            "oauth={} small={} warmup={}",
            cc.oauth_app, cc.small_model, cc.enable_warmup
        );
        widgets::render_field(f, rows[4], "Copilot", &info, false, false);
    }

    // Actions hint at bottom
    let hints = if detail_focused {
        vec![
            Span::styled(
                " e ",
                Style::default()
                    .fg(theme::BG_DARK)
                    .bg(theme::ACCENT2)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Edit  ", widgets::dim_style()),
            Span::styled(
                " ← ",
                Style::default()
                    .fg(theme::BG_DARK)
                    .bg(theme::ACCENT2)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Back to list", widgets::dim_style()),
        ]
    } else {
        vec![Span::styled(
            "  Press → or Enter to edit fields",
            widgets::dim_style(),
        )]
    };
    f.render_widget(Paragraph::new(Line::from(hints)), rows[6]);
}
