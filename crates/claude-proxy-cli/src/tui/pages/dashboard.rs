use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use super::super::app::App;
use super::super::app::LiveModelMetrics;
use super::super::app::ModelCapability;
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

    // Usage overview section (takes remaining space)
    render_model_usage(f, app, rows[2]);
}

fn render_model_usage(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 {
        return;
    }

    let content_area = widgets::render_content_frame(f, area, app, "Usage Overview");

    let Some(ref metrics) = app.live_metrics else {
        let lines = vec![Line::from(Span::styled(
            "   Waiting for server connection...",
            Style::default().fg(theme::FG_DIM),
        ))];
        f.render_widget(Paragraph::new(lines), content_area);
        return;
    };

    let model_list =
        combine_usage_metrics(&metrics.models, metrics.stored.as_ref().map(|s| &s.models));
    let provider_list = combine_usage_metrics(
        &metrics.providers,
        metrics.stored.as_ref().map(|s| &s.providers),
    );
    let initiator_list = combine_usage_metrics(
        &metrics.initiators,
        metrics.stored.as_ref().map(|s| &s.initiators),
    );

    if model_list.is_empty()
        && provider_list.is_empty()
        && initiator_list.is_empty()
        && metrics.model_capabilities.is_empty()
    {
        let lines = vec![Line::from(Span::styled(
            "   No requests recorded yet",
            Style::default().fg(theme::FG_DIM),
        ))];
        f.render_widget(Paragraph::new(lines), content_area);
        return;
    }

    let max_rows = content_area.height as usize;
    let mut lines: Vec<Line> = Vec::new();
    let available = max_rows.saturating_sub(lines.len());
    push_usage_table(&mut lines, "Models", &model_list, available);
    let available = max_rows.saturating_sub(lines.len());
    push_usage_table(&mut lines, "Providers", &provider_list, available);
    let available = max_rows.saturating_sub(lines.len());
    push_usage_table(&mut lines, "Initiators", &initiator_list, available);
    let available = max_rows.saturating_sub(lines.len());
    push_capability_rows(&mut lines, &metrics.model_capabilities, available);

    f.render_widget(Paragraph::new(lines), content_area);
}

fn combine_usage_metrics(
    session: &[(String, LiveModelMetrics)],
    stored: Option<&Vec<(String, LiveModelMetrics)>>,
) -> Vec<(String, LiveModelMetrics)> {
    let mut combined: std::collections::BTreeMap<String, LiveModelMetrics> =
        std::collections::BTreeMap::new();

    for (name, m) in session {
        combined.insert(name.clone(), m.clone());
    }
    if let Some(stored) = stored {
        for (name, m) in stored {
            combined
                .entry(name.clone())
                .and_modify(|e| add_usage(e, m))
                .or_insert_with(|| m.clone());
        }
    }

    let mut items: Vec<(String, LiveModelMetrics)> = combined.into_iter().collect();
    items.sort_by_key(|a| std::cmp::Reverse(a.1.total_tokens()));
    items
}

fn add_usage(target: &mut LiveModelMetrics, source: &LiveModelMetrics) {
    target.requests += source.requests;
    target.input_tokens += source.input_tokens;
    target.output_tokens += source.output_tokens;
    target.cache_creation_input_tokens += source.cache_creation_input_tokens;
    target.cache_read_input_tokens += source.cache_read_input_tokens;
}

fn push_usage_table(
    lines: &mut Vec<Line>,
    title: &str,
    metrics: &[(String, LiveModelMetrics)],
    available: usize,
) {
    if available < 3 || metrics.is_empty() {
        return;
    }
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        format!("   {title}"),
        Style::default().fg(theme::ACCENT),
    )));
    lines.push(Line::from(vec![
        Span::styled(
            format!("   {:<28}", "Name"),
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
            format!("{:>12}", "Total"),
            Style::default().fg(theme::ACCENT2),
        ),
    ]));

    let row_budget = available.saturating_sub(2);
    for (name, m) in metrics.iter().take(row_budget) {
        lines.push(Line::from(vec![
            Span::styled(
                format!("   {:<28}", truncate_name(name, 26)),
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
                format!("{:>12}", format_tokens(m.total_tokens())),
                Style::default().fg(theme::ACCENT),
            ),
        ]));
    }
}

fn push_capability_rows(
    lines: &mut Vec<Line>,
    capabilities: &[(String, ModelCapability)],
    available: usize,
) {
    if available < 3 || capabilities.is_empty() {
        return;
    }
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        "   Model Capabilities",
        Style::default().fg(theme::ACCENT),
    )));
    let row_budget = available.saturating_sub(1);
    for (name, capability) in capabilities.iter().take(row_budget) {
        let max_output = capability
            .max_output_tokens
            .map(format_tokens)
            .unwrap_or_else(|| "-".to_string());
        let flags = capability_flags(capability);
        lines.push(Line::from(vec![
            Span::styled(
                format!("   {:<28}", truncate_name(name, 26)),
                Style::default().fg(theme::FG),
            ),
            Span::styled(
                format!(" {:<10}", capability.provider),
                Style::default().fg(theme::FG_DIM),
            ),
            Span::styled(
                format!(" max {}", max_output),
                Style::default().fg(theme::ACCENT2),
            ),
            Span::styled(format!(" {flags}"), Style::default().fg(theme::FG_DIM)),
        ]));
    }
}

fn capability_flags(capability: &ModelCapability) -> String {
    let mut flags = Vec::new();
    if capability.supports_vision == Some(true) {
        flags.push("vision");
    }
    if capability.supports_thinking == Some(true) {
        flags.push("thinking");
    }
    if capability.supports_adaptive_thinking == Some(true) {
        flags.push("adaptive");
    }
    if !capability.reasoning_effort_levels.is_empty() {
        flags.push("effort");
    }
    if capability
        .supported_endpoints
        .iter()
        .any(|e| e == "/responses")
    {
        flags.push("responses");
    }
    flags.join(",")
}

fn truncate_name(name: &str, max_chars: usize) -> String {
    if name.chars().count() > max_chars {
        format!(
            "{}…",
            name.chars()
                .take(max_chars.saturating_sub(1))
                .collect::<String>()
        )
    } else {
        name.to_string()
    }
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
