use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Paragraph, Row, Table},
};

use super::super::app::{App, Focus, NavItem};
use super::super::{theme, widgets};

pub fn render_settings_page(f: &mut Frame, app: &App, area: Rect) {
    match app.nav {
        NavItem::Server => render_server_page(f, app, area),
        NavItem::Limits => render_limits_page(f, app, area),
        NavItem::Http => render_http_page(f, app, area),
        NavItem::Log => render_log_page(f, app, area),
        NavItem::Model => render_model_page(f, app, area),
        _ => {}
    }
}

fn render_hint(f: &mut Frame, area: Rect, text: &str) {
    let hint = Paragraph::new(Line::from(vec![
        Span::styled("  💡 ", Style::default().fg(theme::FG_DIM)),
        Span::styled(text, Style::default().fg(theme::FG_DIM)),
    ]));
    f.render_widget(hint, area);
}

fn render_server_page(f: &mut Frame, app: &App, area: Rect) {
    let inner = widgets::render_content_frame(f, area, app, "Server");
    let is_focused = matches!(app.focus, Focus::Content);
    let rows = widgets::field_rows(inner, 5);

    widgets::render_field(
        f,
        rows[0],
        "Host",
        &app.settings.server.host,
        is_focused && app.content_idx == 0,
        false,
    );
    widgets::render_field(
        f,
        rows[1],
        "Port",
        &app.settings.server.port.to_string(),
        is_focused && app.content_idx == 1,
        false,
    );
    widgets::render_field(
        f,
        rows[2],
        "Auth Token",
        &app.settings.server.auth_token,
        is_focused && app.content_idx == 2,
        true,
    );
    widgets::render_field(
        f,
        rows[3],
        "Admin Token",
        &app.settings.admin.auth_token.clone().unwrap_or_default(),
        is_focused && app.content_idx == 3,
        true,
    );

    render_hint(
        f,
        rows[4],
        "Admin token: empty = reuse server auth token for admin API",
    );
}

fn render_limits_page(f: &mut Frame, app: &App, area: Rect) {
    let inner = widgets::render_content_frame(f, area, app, "Rate Limits");
    let is_focused = matches!(app.focus, Focus::Content);
    let rows = widgets::field_rows(inner, 5);

    widgets::render_field(
        f,
        rows[0],
        "Rate Limit (req)",
        &app.settings.limits.rate_limit.to_string(),
        is_focused && app.content_idx == 0,
        false,
    );
    widgets::render_field(
        f,
        rows[1],
        "Window (seconds)",
        &app.settings.limits.rate_window.to_string(),
        is_focused && app.content_idx == 1,
        false,
    );
    widgets::render_field(
        f,
        rows[2],
        "Max Concurrency",
        &app.settings.limits.max_concurrency.to_string(),
        is_focused && app.content_idx == 2,
        false,
    );
    widgets::render_field(
        f,
        rows[3],
        "Model Cache TTL (s)",
        &app.settings.limits.model_cache_ttl_seconds.to_string(),
        is_focused && app.content_idx == 3,
        false,
    );

    render_hint(
        f,
        rows[4],
        "Rate limit: max requests allowed per time window",
    );
}

fn render_http_page(f: &mut Frame, app: &App, area: Rect) {
    let inner = widgets::render_content_frame(f, area, app, "HTTP Timeouts");
    let is_focused = matches!(app.focus, Focus::Content);
    let rows = widgets::field_rows(inner, 4);

    widgets::render_field(
        f,
        rows[0],
        "Read Timeout (s)",
        &app.settings.http.read_timeout.to_string(),
        is_focused && app.content_idx == 0,
        false,
    );
    widgets::render_field(
        f,
        rows[1],
        "Write Timeout (s)",
        &app.settings.http.write_timeout.to_string(),
        is_focused && app.content_idx == 1,
        false,
    );
    widgets::render_field(
        f,
        rows[2],
        "Connect Timeout (s)",
        &app.settings.http.connect_timeout.to_string(),
        is_focused && app.content_idx == 2,
        false,
    );

    render_hint(
        f,
        rows[3],
        "Timeouts in seconds for upstream HTTP connections",
    );
}

fn render_log_page(f: &mut Frame, app: &App, area: Rect) {
    let inner = widgets::render_content_frame(f, area, app, "Logging");
    let is_focused = matches!(app.focus, Focus::Content);
    let rows = widgets::field_rows(inner, 4);

    widgets::render_field(
        f,
        rows[0],
        "Level",
        &app.settings.log.level,
        is_focused && app.content_idx == 0,
        false,
    );
    widgets::render_toggle(
        f,
        rows[1],
        "Raw API Payloads",
        app.settings.log.raw_api_payloads,
        is_focused && app.content_idx == 1,
    );
    widgets::render_toggle(
        f,
        rows[2],
        "Raw SSE Events",
        app.settings.log.raw_sse_events,
        is_focused && app.content_idx == 2,
    );

    render_hint(
        f,
        rows[3],
        "Level: trace | debug | info | warn | error. Space to toggle",
    );
}

fn render_model_page(f: &mut Frame, app: &App, area: Rect) {
    let inner = widgets::render_content_frame(f, area, app, "Model Aliases");
    let is_focused = matches!(app.focus, Focus::Content);
    let rows_area = widgets::field_rows(inner, 6);

    let aliases = [
        (
            "Default",
            app.settings.model.default.name.as_str(),
            app.settings
                .model
                .default
                .reasoning_effort
                .map(|effort| effort.as_config_value())
                .unwrap_or("(unset)"),
        ),
        (
            "Reasoning",
            app.settings.model.reasoning_name().unwrap_or("(none)"),
            app.settings
                .model
                .reasoning
                .as_ref()
                .and_then(|alias| alias.reasoning_effort)
                .map(|effort| effort.as_config_value())
                .unwrap_or("(unset)"),
        ),
        (
            "Opus",
            app.settings.model.opus_name().unwrap_or("(none)"),
            app.settings
                .model
                .opus
                .as_ref()
                .and_then(|alias| alias.reasoning_effort)
                .map(|effort| effort.as_config_value())
                .unwrap_or("(unset)"),
        ),
        (
            "Sonnet",
            app.settings.model.sonnet_name().unwrap_or("(none)"),
            app.settings
                .model
                .sonnet
                .as_ref()
                .and_then(|alias| alias.reasoning_effort)
                .map(|effort| effort.as_config_value())
                .unwrap_or("(unset)"),
        ),
        (
            "Haiku",
            app.settings.model.haiku_name().unwrap_or("(none)"),
            app.settings
                .model
                .haiku
                .as_ref()
                .and_then(|alias| alias.reasoning_effort)
                .map(|effort| effort.as_config_value())
                .unwrap_or("(unset)"),
        ),
    ];

    let table_rows = aliases
        .iter()
        .enumerate()
        .map(|(idx, (label, model, effort))| {
            let row_selected = is_focused && app.content_idx == idx;
            let label_style = if row_selected {
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::FG_DIM)
            };
            let model_style = cell_style(row_selected && app.detail_idx == 0);
            let effort_style = cell_style(row_selected && app.detail_idx == 1);
            Row::new(vec![
                Cell::from(format!(" {}", label)).style(label_style),
                Cell::from((*model).to_string()).style(model_style),
                Cell::from((*effort).to_string()).style(effort_style),
            ])
        });

    let table = Table::new(
        table_rows,
        [
            Constraint::Length(14),
            Constraint::Percentage(56),
            Constraint::Percentage(24),
        ],
    )
    .header(
        Row::new(vec![" Alias", "Model", "Reasoning Effort"]).style(
            Style::default()
                .fg(theme::FG_DIM)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .column_spacing(2);
    f.render_widget(table, rows_area[0].union(rows_area[4]));

    render_hint(
        f,
        rows_area[5],
        "Model: provider_id/model_name. Reasoning: unset | default | none | low | medium | high | xhigh",
    );
}

fn cell_style(is_selected: bool) -> Style {
    if is_selected {
        Style::default()
            .fg(theme::BG_DARK)
            .bg(theme::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::FG)
    }
}
