use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use super::super::app::App;
use super::super::app::LiveModelMetrics;
use super::super::{theme, widgets};

pub fn render_dashboard(f: &mut Frame, app: &App, area: Rect) {
    // Three-row layout for dashboard cards
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Min(0),
        ])
        .split(area);

    // Top row: Overview + Metrics side by side
    let top_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);

    // Bottom row: Server + Limits side by side
    let bot_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    // Overview card
    let overview = widgets::render_content_frame(f, top_cols[0], app, "Overview");
    let provider_count = app.settings.providers.len();
    let provider_names: Vec<&str> = app.settings.providers.keys().map(|s| s.as_str()).collect();
    render_card_details(
        f,
        overview,
        &[
            ("Providers", &format!("{provider_count} configured")),
            ("Names", &provider_names.join(", ")),
            ("Default", &app.settings.model.default),
        ],
    );

    // Live Metrics card
    let metrics_area = widgets::render_content_frame(f, top_cols[1], app, "Live Metrics");
    if let Some(ref metrics) = app.live_metrics {
        let avg_lat = format!("{}ms", metrics.avg_latency_ms);
        if let Some(ref stored) = metrics.stored {
            let total_reqs = metrics.requests_total + stored.requests_total;
            let total_errs = metrics.errors_total + stored.errors_total;
            render_card_details(
                f,
                metrics_area,
                &[
                    (
                        "Requests",
                        &format!("{total_reqs} (session: {})", metrics.requests_total),
                    ),
                    (
                        "Errors",
                        &format!("{total_errs} (session: {})", metrics.errors_total),
                    ),
                    ("Avg Latency", &avg_lat),
                ],
            );
        } else {
            render_card_details(
                f,
                metrics_area,
                &[
                    ("Requests", &format!("{}", metrics.requests_total)),
                    ("Errors", &format!("{}", metrics.errors_total)),
                    ("Avg Latency", &avg_lat),
                ],
            );
        }
    } else {
        render_card_details(
            f,
            metrics_area,
            &[("Status", "connecting..."), ("", ""), ("", "")],
        );
    }

    // Server card
    let server_area = widgets::render_content_frame(f, bot_cols[0], app, "Server");
    render_card_details(
        f,
        server_area,
        &[
            (
                "Listen",
                &format!("{}:{}", app.settings.server.host, app.settings.server.port),
            ),
            (
                "Auth Token",
                &widgets::mask_value(&app.settings.server.auth_token),
            ),
            (
                "Admin Token",
                app.settings
                    .admin
                    .auth_token
                    .as_deref()
                    .map(|_| "set")
                    .unwrap_or("(none)"),
            ),
        ],
    );

    // Limits card
    let limits_area = widgets::render_content_frame(f, bot_cols[1], app, "Rate Limits");
    render_card_details(
        f,
        limits_area,
        &[
            (
                "Rate Limit",
                &format!(
                    "{} req / {}s",
                    app.settings.limits.rate_limit, app.settings.limits.rate_window
                ),
            ),
            (
                "Concurrency",
                &app.settings.limits.max_concurrency.to_string(),
            ),
        ],
    );

    // Model Token Usage section (takes remaining space)
    render_model_usage(f, app, rows[2]);
}

fn render_model_usage(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 {
        return;
    }

    let content_area = widgets::render_content_frame(f, area, app, "Model Token Usage");

    let Some(ref metrics) = app.live_metrics else {
        let lines = vec![Line::from(Span::styled(
            "   Waiting for server connection...",
            Style::default().fg(theme::FG_DIM),
        ))];
        f.render_widget(Paragraph::new(lines), content_area);
        return;
    };

    // Merge session + stored model metrics into combined totals
    let mut combined: std::collections::BTreeMap<String, LiveModelMetrics> =
        std::collections::BTreeMap::new();

    for (name, m) in &metrics.models {
        combined.insert(name.clone(), m.clone());
    }
    if let Some(ref stored) = metrics.stored {
        for (name, m) in &stored.models {
            combined
                .entry(name.clone())
                .and_modify(|e| {
                    e.requests += m.requests;
                    e.input_tokens += m.input_tokens;
                    e.output_tokens += m.output_tokens;
                    e.cache_creation_input_tokens += m.cache_creation_input_tokens;
                    e.cache_read_input_tokens += m.cache_read_input_tokens;
                })
                .or_insert_with(|| m.clone());
        }
    }

    if combined.is_empty() {
        let lines = vec![Line::from(Span::styled(
            "   No requests recorded yet",
            Style::default().fg(theme::FG_DIM),
        ))];
        f.render_widget(Paragraph::new(lines), content_area);
        return;
    }

    // Sort by total tokens descending
    let mut model_list: Vec<(String, LiveModelMetrics)> = combined.into_iter().collect();
    model_list.sort_by_key(|a| std::cmp::Reverse(a.1.total_tokens()));

    // Header line
    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled(
            format!("   {:<30}", "Model"),
            Style::default().fg(theme::ACCENT2),
        ),
        Span::styled(
            format!("{:>8}", "Reqs"),
            Style::default().fg(theme::ACCENT2),
        ),
        Span::styled(
            format!("{:>12}", "Input"),
            Style::default().fg(theme::ACCENT2),
        ),
        Span::styled(
            format!("{:>12}", "Output"),
            Style::default().fg(theme::ACCENT2),
        ),
        Span::styled(
            format!("{:>12}", "Cache R"),
            Style::default().fg(theme::ACCENT2),
        ),
        Span::styled(
            format!("{:>12}", "Cache W"),
            Style::default().fg(theme::ACCENT2),
        ),
        Span::styled(
            format!("{:>12}", "Total"),
            Style::default().fg(theme::ACCENT2),
        ),
    ])];

    // Data rows
    let max_rows = (content_area.height as usize).saturating_sub(1);
    for (name, m) in model_list.iter().take(max_rows) {
        let display_name = if name.len() > 28 {
            format!("{}…", &name[..27])
        } else {
            name.clone()
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("   {:<30}", display_name),
                Style::default().fg(theme::FG),
            ),
            Span::styled(
                format!("{:>8}", format_number(m.requests)),
                Style::default().fg(theme::ACCENT),
            ),
            Span::styled(
                format!("{:>12}", format_tokens(m.input_tokens)),
                Style::default().fg(theme::FG),
            ),
            Span::styled(
                format!("{:>12}", format_tokens(m.output_tokens)),
                Style::default().fg(theme::FG),
            ),
            Span::styled(
                format!("{:>12}", format_tokens(m.cache_read_input_tokens)),
                Style::default().fg(theme::FG_DIM),
            ),
            Span::styled(
                format!("{:>12}", format_tokens(m.cache_creation_input_tokens)),
                Style::default().fg(theme::FG_DIM),
            ),
            Span::styled(
                format!("{:>12}", format_tokens(m.total_tokens())),
                Style::default().fg(theme::ACCENT),
            ),
        ]));
    }

    f.render_widget(Paragraph::new(lines), content_area);
}

fn render_card_details(f: &mut Frame, area: Rect, rows: &[(&str, &str)]) {
    let lines: Vec<Line> = rows
        .iter()
        .filter(|(k, _)| !k.is_empty())
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("   {:<14}", k), Style::default().fg(theme::FG_DIM)),
                Span::styled(format!(" {}", v), Style::default().fg(theme::ACCENT)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

/// Format token count with K/M suffix for readability.
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Format number with commas.
fn format_number(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
