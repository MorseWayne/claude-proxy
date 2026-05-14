use std::path::PathBuf;

use claude_proxy_config::settings::LogConfig;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Initialize the tracing subscriber with dual output: file + optional stderr.
///
/// - Always writes to a log file (via non-blocking tracing_appender).
/// - If `log_config.with_stdout` is true, also writes to stderr (for non-daemon server).
/// - In TUI mode, call with `with_stdout = false` to avoid corrupting the terminal.
pub fn init_logging(log_config: &LogConfig, tui_mode: bool) -> anyhow::Result<()> {
    let log_dir = log_dir();
    std::fs::create_dir_all(&log_dir)?;

    let log_file = log_config
        .file
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| log_dir.join("claude-proxy.log"));

    // Non-blocking file writer (flushes every second, buffer 128k lines)
    let file_appender = tracing_appender::rolling::never(
        log_file.parent().unwrap_or(&log_dir),
        log_file
            .file_name()
            .unwrap_or(std::ffi::OsStr::new("claude-proxy.log")),
    );
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Leak the guard so it lives for the process lifetime (it flushes on drop)
    std::mem::forget(_guard);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_writer(non_blocking)
        .with_filter(build_filter(log_config));

    let level = log_config.level.clone();

    if !tui_mode && log_config.with_stdout {
        // Dual output: file + stderr
        let stderr_layer = tracing_subscriber::fmt::layer()
            .with_ansi(true)
            .with_target(false)
            .with_writer(std::io::stderr)
            .with_filter(build_filter(log_config));

        tracing_subscriber::registry()
            .with(file_layer)
            .with(stderr_layer)
            .init();
    } else {
        // File only (for TUI or daemon mode)
        tracing_subscriber::registry().with(file_layer).init();
    }

    tracing::info!(
        "Logging initialized: level={level}, file={}",
        log_file.display()
    );
    if !tui_mode && log_config.with_stdout {
        tracing::info!("Dual output: file + stderr");
    }

    Ok(())
}

fn build_filter(log_config: &LogConfig) -> EnvFilter {
    // Respect RUST_LOG env var first, then fall back to config
    EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = &log_config.level;
        EnvFilter::new(format!("{level},hyper=warn,reqwest=warn,tower_http=info"))
    })
}

fn log_dir() -> PathBuf {
    claude_proxy_config::Settings::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("logs")
}
