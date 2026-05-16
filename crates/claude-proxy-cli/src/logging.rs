use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use claude_proxy_config::settings::LogConfig;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
const MAX_LOG_FILES: usize = 5;

struct RotatingLogWriter {
    inner: Mutex<RotatingLogState>,
}

struct RotatingLogState {
    path: PathBuf,
    max_bytes: u64,
    max_files: usize,
    file: File,
}

impl RotatingLogWriter {
    fn new(path: PathBuf, max_bytes: u64, max_files: usize) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        rotate_if_needed(&path, max_bytes, max_files)?;
        let file = open_log_file(&path)?;
        Ok(Self {
            inner: Mutex::new(RotatingLogState {
                path,
                max_bytes,
                max_files,
                file,
            }),
        })
    }
}

impl Write for RotatingLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("rotating log writer lock poisoned"))?;
        let current_size = state.file.metadata().map(|m| m.len()).unwrap_or(0);
        if current_size.saturating_add(buf.len() as u64) > state.max_bytes {
            state.file.flush()?;
            rotate_files(&state.path, state.max_files)?;
            state.file = open_log_file(&state.path)?;
        }
        state.file.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("rotating log writer lock poisoned"))?;
        state.file.flush()
    }
}

fn open_log_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn rotate_if_needed(path: &Path, max_bytes: u64, max_files: usize) -> io::Result<()> {
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) >= max_bytes {
        rotate_files(path, max_files)?;
    }
    Ok(())
}

fn rotate_files(path: &Path, max_files: usize) -> io::Result<()> {
    if max_files == 0 {
        let _ = std::fs::remove_file(path);
        return Ok(());
    }

    let oldest = rotated_path(path, max_files);
    let _ = std::fs::remove_file(oldest);
    for index in (1..max_files).rev() {
        let src = rotated_path(path, index);
        let dst = rotated_path(path, index + 1);
        if src.exists() {
            std::fs::rename(src, dst)?;
        }
    }
    if path.exists() {
        std::fs::rename(path, rotated_path(path, 1))?;
    }
    Ok(())
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    PathBuf::from(format!("{}.{}", path.display(), index))
}

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
    let rotating_writer = RotatingLogWriter::new(log_file.clone(), MAX_LOG_BYTES, MAX_LOG_FILES)?;
    let (non_blocking, _guard) = tracing_appender::non_blocking(rotating_writer);

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
    let base = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = &log_config.level;
        EnvFilter::new(level)
    });

    base.add_directive("hickory_proto=warn".parse().expect("valid filter"))
        .add_directive("hickory_resolver=warn".parse().expect("valid filter"))
        .add_directive("hyper=warn".parse().expect("valid filter"))
        .add_directive("hyper_util=warn".parse().expect("valid filter"))
        .add_directive("reqwest=warn".parse().expect("valid filter"))
        .add_directive("tower_http=info".parse().expect("valid filter"))
}

fn log_dir() -> PathBuf {
    claude_proxy_config::Settings::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("logs")
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_log_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("claude-proxy-log-{name}-{nanos}"))
            .join("app.log")
    }

    #[test]
    fn rotate_files_keeps_bounded_history() {
        let path = temp_log_path("bounded");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "current").unwrap();
        std::fs::write(rotated_path(&path, 1), "one").unwrap();
        std::fs::write(rotated_path(&path, 2), "two").unwrap();

        rotate_files(&path, 2).unwrap();

        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(rotated_path(&path, 1)).unwrap(),
            "current"
        );
        assert_eq!(
            std::fs::read_to_string(rotated_path(&path, 2)).unwrap(),
            "one"
        );
        assert!(!rotated_path(&path, 3).exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rotating_writer_rotates_when_limit_is_exceeded() {
        let path = temp_log_path("writer");
        let mut writer = RotatingLogWriter::new(path.clone(), 8, 2).unwrap();

        writer.write_all(b"12345678").unwrap();
        writer.write_all(b"abc").unwrap();
        writer.flush().unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "abc");
        assert_eq!(
            std::fs::read_to_string(rotated_path(&path, 1)).unwrap(),
            "12345678"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
