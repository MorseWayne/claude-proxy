use std::collections::BTreeMap;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use super::super::app::App;
use super::super::app::ErrorDiagnostics;
use super::super::app::LiveModelMetrics;
use super::super::app::ModelCapability;
use super::super::app::ObservabilitySummary;
use super::super::{theme, widgets};
use claude_proxy_providers::provider::{RateLimitSnapshot, RateLimitSource, RateLimitWindow};

pub fn render_dashboard(f: &mut Frame, app: &App, area: Rect) {
    // Four-row layout for dashboard cards
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(6),
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
            ("Default", &app.settings.model.default.name),
        ],
    );

    // Live Metrics card
    let metrics_area = widgets::render_content_frame(f, top_cols[1], app, "Live Metrics");
    if let Some(ref metrics) = app.live_metrics {
        let avg_lat = format!("{}ms", metrics.avg_latency_ms);
        let observability = observability_status(&metrics.observability.summary);
        let errors = combined_error_status(
            &metrics.diagnostics,
            metrics.stored.as_ref().map(|stored| &stored.diagnostics),
        );
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
                    ("Observe", &observability),
                    ("Top Error", &errors),
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
                    ("Observe", &observability),
                    ("Top Error", &errors),
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
                    .unwrap_or("fallback to auth"),
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

    // ChatGPT quota card
    render_rate_limit_card(f, app, rows[2]);

    // Usage overview section (takes remaining space)
    render_model_usage(f, app, rows[3]);
}

fn render_rate_limit_card(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 {
        return;
    }

    let content_area = widgets::render_content_frame(f, area, app, "ChatGPT / Codex Quota");
    let Some(ref metrics) = app.live_metrics else {
        let lines = vec![Line::from(Span::styled(
            "   Waiting for server connection...",
            Style::default().fg(theme::FG_DIM),
        ))];
        f.render_widget(Paragraph::new(lines), content_area);
        return;
    };

    if metrics.provider_rate_limits.is_empty() {
        let lines = vec![Line::from(Span::styled(
            "   Not available yet — login required or waiting for quota data",
            Style::default().fg(theme::FG_DIM),
        ))];
        f.render_widget(Paragraph::new(lines), content_area);
        return;
    }

    let mut lines = Vec::new();
    push_rate_limit_rows(
        &mut lines,
        &metrics.provider_rate_limits,
        content_area.height as usize,
        false,
    );
    f.render_widget(Paragraph::new(lines), content_area);
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

fn observability_status(summary: &ObservabilitySummary) -> String {
    if summary.requests == 0 {
        return "no samples".to_string();
    }
    format!(
        "e2e {}ms · up {}ms · gap {}ms",
        summary.avg_total_latency_ms, summary.avg_upstream_connect_ms, summary.max_event_gap_ms
    )
}

fn combined_error_status(session: &ErrorDiagnostics, stored: Option<&ErrorDiagnostics>) -> String {
    let total_errors = session.errors + stored.map(|diagnostics| diagnostics.errors).unwrap_or(0);
    if total_errors == 0 {
        return "none".to_string();
    }

    let mut error_kinds = session.error_kinds.clone();
    let mut terminal_reasons = session.terminal_reasons.clone();
    if let Some(stored) = stored {
        add_counts(&mut error_kinds, &stored.error_kinds);
        add_counts(&mut terminal_reasons, &stored.terminal_reasons);
    }

    if let Some((kind, count)) = top_count(&error_kinds) {
        return format!("kind {kind} ({count})");
    }
    if let Some((reason, count)) = top_count(&terminal_reasons) {
        return format!("reason {reason} ({count})");
    }
    format!("{total_errors} errors")
}

fn add_counts(target: &mut BTreeMap<String, u64>, source: &BTreeMap<String, u64>) {
    for (key, value) in source {
        *target.entry(key.clone()).or_default() += value;
    }
}

fn top_count(items: &BTreeMap<String, u64>) -> Option<(&str, u64)> {
    items
        .iter()
        .max_by_key(|(key, value)| (*value, std::cmp::Reverse((*key).as_str())))
        .map(|(key, value)| (key.as_str(), *value))
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

fn push_rate_limit_rows(
    lines: &mut Vec<Line>,
    provider_rate_limits: &[(String, Vec<RateLimitSnapshot>)],
    available: usize,
    show_title: bool,
) {
    if available < 2 || provider_rate_limits.is_empty() {
        return;
    }
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    if show_title {
        lines.push(Line::from(Span::styled(
            "   ChatGPT / Codex Quota",
            Style::default().fg(theme::ACCENT),
        )));
    }

    let row_budget = available.saturating_sub(usize::from(show_title));
    let mut used_rows = 0;
    for (provider_id, snapshots) in provider_rate_limits {
        for snapshot in snapshots {
            if used_rows >= row_budget {
                return;
            }
            lines.push(Line::from(vec![
                Span::styled(
                    format!("   {:<18}", truncate_name(provider_id, 16)),
                    Style::default().fg(theme::FG),
                ),
                Span::styled(
                    format!(" {:<14}", truncate_name(&rate_limit_label(snapshot), 12)),
                    Style::default().fg(theme::ACCENT2),
                ),
                Span::styled(
                    format!(
                        " 5h {:>7}",
                        format_rate_limit_window(snapshot.primary.as_ref())
                    ),
                    Style::default().fg(theme::FG),
                ),
                Span::styled(
                    format!(
                        " weekly {:>7}",
                        format_rate_limit_window(snapshot.secondary.as_ref())
                    ),
                    Style::default().fg(theme::FG),
                ),
                Span::styled(
                    format!(" {}", format_rate_limit_extra(snapshot)),
                    Style::default().fg(theme::FG_DIM),
                ),
            ]));
            used_rows += 1;
        }
    }
}

fn rate_limit_label(snapshot: &RateLimitSnapshot) -> String {
    snapshot
        .limit_name
        .clone()
        .or_else(|| snapshot.feature.clone())
        .unwrap_or_else(|| "codex".to_string())
}

fn format_rate_limit_window(window: Option<&RateLimitWindow>) -> String {
    window
        .map(|window| format!("{:>5.1}%", window.used_percent))
        .unwrap_or_else(|| "-".to_string())
}

fn format_rate_limit_extra(snapshot: &RateLimitSnapshot) -> String {
    let mut parts = Vec::new();
    if let Some(credits) = snapshot.credits.as_ref() {
        if credits.unlimited == Some(true) {
            parts.push("credits unlimited".to_string());
        } else if let Some(balance) = credits.balance.as_deref() {
            parts.push(format!("credits {balance}"));
        } else if credits.has_credits == Some(false) {
            parts.push("no credits".to_string());
        }
    }
    if let Some(plan_type) = snapshot.plan_type.as_ref() {
        parts.push(plan_type.clone());
    }
    parts.push(match snapshot.source {
        RateLimitSource::UsageEndpoint => "usage".to_string(),
        RateLimitSource::ResponseHeaders => "headers".to_string(),
        RateLimitSource::StreamEvent => "stream".to_string(),
    });
    parts.join(" · ")
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
        let context_window = capability
            .context_window
            .map(format_tokens)
            .unwrap_or_else(|| "-".to_string());
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
                format!(" ctx {:>7}", context_window),
                Style::default().fg(theme::ACCENT2),
            ),
            Span::styled(
                format!(" out {:>7}", max_output),
                Style::default().fg(theme::ACCENT2),
            ),
            Span::styled(format!(" {flags}"), Style::default().fg(theme::FG_DIM)),
        ]));
    }
}

fn capability_flags(capability: &ModelCapability) -> String {
    let mut flags = Vec::new();
    if capability.streaming {
        flags.push("stream");
    }
    if capability.tools {
        flags.push("tools");
    }
    if capability.vision {
        flags.push("vision");
    }
    if capability.thinking {
        flags.push("think");
    }
    if capability.adaptive_thinking {
        flags.push("adaptive");
    }
    if !capability.reasoning_effort_levels.is_empty() {
        flags.push("effort");
    }
    if capability.prompt_cache {
        match capability.prompt_cache_scope.as_deref() {
            Some("global_scope") => flags.push("cache:g"),
            Some("basic") => flags.push("cache"),
            _ => flags.push("cache"),
        }
    }
    if capability.tool_search {
        flags.push("search");
    }
    if capability.structured_outputs {
        flags.push("json");
    }
    if capability.strict_tools {
        flags.push("strict");
    }
    if capability.fast_mode {
        flags.push("fast");
    }
    if let Some(mode) = capability.token_counting_mode.as_deref()
        && mode != "unknown"
        && mode != "none"
    {
        flags.push(match mode {
            "native" => "tok:n",
            "rough" => "tok:r",
            _ => "tok",
        });
    }
    if capability
        .supported_endpoints
        .iter()
        .any(|e| e == "/responses")
    {
        flags.push("resp");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_extra_labels_stream_event_source() {
        let snapshot = RateLimitSnapshot {
            source: RateLimitSource::StreamEvent,
            ..Default::default()
        };

        assert_eq!(format_rate_limit_extra(&snapshot), "stream");
    }

    #[test]
    fn combined_error_status_prefers_top_error_kind() {
        let session = ErrorDiagnostics {
            errors: 2,
            error_kinds: BTreeMap::from([("stream".to_string(), 2)]),
            ..Default::default()
        };
        let stored = ErrorDiagnostics {
            errors: 3,
            error_kinds: BTreeMap::from([("rate_limited".to_string(), 3)]),
            ..Default::default()
        };

        assert_eq!(
            combined_error_status(&session, Some(&stored)),
            "kind rate_limited (3)"
        );
    }

    #[test]
    fn observability_status_formats_latency_summary() {
        let summary = ObservabilitySummary {
            requests: 1,
            avg_total_latency_ms: 1200,
            avg_upstream_connect_ms: 300,
            max_event_gap_ms: 450,
            ..Default::default()
        };

        assert_eq!(
            observability_status(&summary),
            "e2e 1200ms · up 300ms · gap 450ms"
        );
    }
}
