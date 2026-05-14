use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use super::super::app::App;
use super::super::{theme, widgets};

pub fn render_system_page(f: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Min(0),
        ])
        .split(area);

    // Paths card
    let paths_area = widgets::render_content_frame(f, rows[0], app, "Paths");
    let config_path = claude_proxy_config::Settings::config_file_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(not found)".to_string());
    let config_dir = claude_proxy_config::Settings::config_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(not found)".to_string());
    let log_path = app.settings.log.file.clone().unwrap_or_else(|| {
        claude_proxy_config::Settings::config_dir()
            .map(|p| {
                p.join("logs")
                    .join("claude-proxy.log")
                    .display()
                    .to_string()
            })
            .unwrap_or_else(|| "(default)".to_string())
    });
    let log_dir = claude_proxy_config::Settings::config_dir()
        .map(|p| p.join("logs").display().to_string())
        .unwrap_or_else(|| "(unknown)".to_string());

    render_info_rows(
        f,
        paths_area,
        &[
            ("Config File", &config_path),
            ("Config Dir", &config_dir),
            ("Log File", &log_path),
            ("Log Dir", &log_dir),
        ],
    );

    // Runtime card
    let runtime_area = widgets::render_content_frame(f, rows[1], app, "Runtime");
    let pid = std::process::id().to_string();
    let version = env!("CARGO_PKG_VERSION");
    let server_addr = format!("{}:{}", app.settings.server.host, app.settings.server.port);
    let status = if app.live_metrics.is_some() {
        "● running"
    } else {
        "○ not connected"
    };

    render_info_rows(
        f,
        runtime_area,
        &[
            ("Version", version),
            ("PID", &pid),
            ("Server", &server_addr),
            ("Status", status),
        ],
    );

    // Environment card
    if rows[2].height >= 4 {
        let env_area = widgets::render_content_frame(f, rows[2], app, "Environment");
        let rust_log = std::env::var("RUST_LOG").unwrap_or_else(|_| "(not set)".to_string());
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;

        render_info_rows(
            f,
            env_area,
            &[
                ("OS", &format!("{os}/{arch}")),
                ("RUST_LOG", &rust_log),
                ("Log Level", &app.settings.log.level),
                (
                    "Log Stdout",
                    if app.settings.log.with_stdout {
                        "on"
                    } else {
                        "off"
                    },
                ),
            ],
        );
    }
}

fn render_info_rows(f: &mut Frame, area: Rect, rows: &[(&str, &str)]) {
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("   {:<14}", k), Style::default().fg(theme::FG_DIM)),
                Span::styled(format!(" {}", v), Style::default().fg(theme::ACCENT)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}
