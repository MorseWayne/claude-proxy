use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::app::{
    ErrorDiagnostics, RequestObservabilityEvent, RequestObservabilityStored, TokenUsage,
    UsageMetrics,
};

const METRICS_RETENTION_DAYS: i64 = 90;
const METRICS_WRITE_QUEUE_CAPACITY: usize = 4096;
const METRICS_MAINTENANCE_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(24 * 60 * 60);

enum MetricsWriteEvent {
    Usage(UsageEvent),
    Observability(Box<RequestObservabilityEvent>),
}

/// A single write event to be persisted.
struct UsageEvent {
    provider: String,
    initiator: String,
    model: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_input_tokens: i64,
    cache_read_input_tokens: i64,
    is_error: i64,
    latency_ms: i64,
    terminal_reason: String,
    error_kind: String,
}

/// A completed request usage record to be persisted and aggregated.
pub struct CompletedUsageRecord<'a> {
    pub provider: &'a str,
    pub initiator: &'a str,
    pub model: &'a str,
    pub usage: &'a TokenUsage,
    pub is_error: bool,
    pub latency_ms: u64,
    pub terminal_reason: &'a str,
    pub error_kind: &'a str,
}

fn ensure_usage_events_diagnostic_columns(conn: &Connection) -> rusqlite::Result<()> {
    if !usage_events_has_column(conn, "terminal_reason")? {
        conn.execute(
            "ALTER TABLE usage_events ADD COLUMN terminal_reason TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    if !usage_events_has_column(conn, "error_kind")? {
        conn.execute(
            "ALTER TABLE usage_events ADD COLUMN error_kind TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    Ok(())
}

fn usage_events_has_column(conn: &Connection, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare("PRAGMA table_info(usage_events)")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in rows.flatten() {
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn rebuild_usage_events_schema(conn: &Connection) -> rusqlite::Result<()> {
    let has_current_schema = conn
        .prepare("SELECT provider, initiator FROM usage_events LIMIT 0")
        .is_ok();

    if !has_current_schema {
        conn.execute_batch("DROP TABLE IF EXISTS usage_events;")?;
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            provider TEXT NOT NULL,
            initiator TEXT NOT NULL,
            model TEXT NOT NULL,
            input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
            is_error INTEGER NOT NULL DEFAULT 0,
            latency_ms INTEGER NOT NULL DEFAULT 0,
            terminal_reason TEXT NOT NULL DEFAULT '',
            error_kind TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_usage_events_provider ON usage_events(provider);
        CREATE INDEX IF NOT EXISTS idx_usage_events_initiator ON usage_events(initiator);
        CREATE INDEX IF NOT EXISTS idx_usage_events_model ON usage_events(model);
        CREATE INDEX IF NOT EXISTS idx_usage_events_created ON usage_events(created_at);",
    )?;
    ensure_usage_events_diagnostic_columns(conn)
}

fn ensure_request_observability_columns(conn: &Connection) -> rusqlite::Result<()> {
    for (name, definition) in [
        ("transport", "transport TEXT"),
        ("websocket_reused", "websocket_reused INTEGER"),
        ("continuation_used", "continuation_used INTEGER"),
        (
            "continuation_disabled_reason",
            "continuation_disabled_reason TEXT",
        ),
        (
            "continuation_fallback_used",
            "continuation_fallback_used INTEGER",
        ),
        ("fallback_reason", "fallback_reason TEXT"),
        ("upstream_error_status", "upstream_error_status INTEGER"),
        ("upstream_error_code", "upstream_error_code TEXT"),
        (
            "upstream_error_message_class",
            "upstream_error_message_class TEXT",
        ),
        (
            "request_body_bytes",
            "request_body_bytes INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "upstream_send_body_bytes",
            "upstream_send_body_bytes INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "continuation_saved_bytes",
            "continuation_saved_bytes INTEGER NOT NULL DEFAULT 0",
        ),
        ("responses_lite", "responses_lite INTEGER"),
    ] {
        if !request_observability_events_has_column(conn, name)? {
            conn.execute(
                &format!("ALTER TABLE request_observability_events ADD COLUMN {definition}"),
                [],
            )?;
        }
    }
    Ok(())
}

fn request_observability_events_has_column(
    conn: &Connection,
    column: &str,
) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare("PRAGMA table_info(request_observability_events)")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in rows.flatten() {
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn rebuild_request_observability_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS request_observability_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            request_id TEXT NOT NULL,
            provider TEXT NOT NULL,
            initiator TEXT NOT NULL,
            model TEXT NOT NULL,
            stream INTEGER NOT NULL DEFAULT 0,
            is_error INTEGER NOT NULL DEFAULT 0,
            terminal_reason TEXT NOT NULL,
            total_latency_ms INTEGER NOT NULL DEFAULT 0,
            provider_setup_ms INTEGER NOT NULL DEFAULT 0,
            upstream_connect_ms INTEGER NOT NULL DEFAULT 0,
            stream_duration_ms INTEGER NOT NULL DEFAULT 0,
            first_event_ms INTEGER,
            last_event_gap_ms INTEGER NOT NULL DEFAULT 0,
            max_event_gap_ms INTEGER NOT NULL DEFAULT 0,
            idle_gap_count INTEGER NOT NULL DEFAULT 0,
            event_count INTEGER NOT NULL DEFAULT 0,
            transport TEXT,
            websocket_reused INTEGER,
            continuation_used INTEGER,
            continuation_disabled_reason TEXT,
            continuation_fallback_used INTEGER,
            fallback_reason TEXT,
            upstream_error_status INTEGER,
            upstream_error_code TEXT,
            upstream_error_message_class TEXT,
            request_body_bytes INTEGER NOT NULL DEFAULT 0,
            upstream_send_body_bytes INTEGER NOT NULL DEFAULT 0,
            continuation_saved_bytes INTEGER NOT NULL DEFAULT 0,
            responses_lite INTEGER,
            prompt_too_long_retries INTEGER NOT NULL DEFAULT 0,
            prompt_too_long_original_body_bytes INTEGER NOT NULL DEFAULT 0,
            prompt_too_long_shrunk_body_bytes INTEGER NOT NULL DEFAULT 0,
            prompt_too_long_dropped_items INTEGER NOT NULL DEFAULT 0,
            request_messages INTEGER NOT NULL DEFAULT 0,
            request_content_blocks INTEGER NOT NULL DEFAULT 0,
            request_tool_results INTEGER NOT NULL DEFAULT 0,
            request_text_bytes INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_request_observability_events_provider ON request_observability_events(provider);
        CREATE INDEX IF NOT EXISTS idx_request_observability_events_model ON request_observability_events(model);
        CREATE INDEX IF NOT EXISTS idx_request_observability_events_created ON request_observability_events(created_at);",
    )?;
    ensure_request_observability_columns(conn)
}

fn initialize_metrics_schema(conn: &Connection) -> rusqlite::Result<()> {
    rebuild_usage_events_schema(conn)?;
    rebuild_request_observability_schema(conn)?;
    Ok(())
}

fn run_metrics_maintenance(conn: &Connection) -> rusqlite::Result<()> {
    prune_old_usage_events(conn, METRICS_RETENTION_DAYS)?;
    prune_old_request_observability_events(conn, METRICS_RETENTION_DAYS)?;
    checkpoint_metrics_wal(conn)?;
    Ok(())
}

fn prune_old_usage_events(conn: &Connection, retention_days: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM usage_events WHERE created_at < datetime('now', ?1)",
        [format!("-{retention_days} days")],
    )
}

fn prune_old_request_observability_events(
    conn: &Connection,
    retention_days: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM request_observability_events WHERE created_at < datetime('now', ?1)",
        [format!("-{retention_days} days")],
    )
}

fn checkpoint_metrics_wal(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
}

fn spawn_metrics_maintenance(conn: Arc<std::sync::Mutex<Connection>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(METRICS_MAINTENANCE_INTERVAL);
        loop {
            interval.tick().await;
            let conn = conn.clone();
            let result = tokio::task::spawn_blocking(move || {
                let conn = conn.lock().unwrap();
                run_metrics_maintenance(&conn)
            })
            .await;

            match result {
                Ok(Ok(())) => info!("Metrics maintenance completed"),
                Ok(Err(e)) => warn!("Metrics maintenance failed: {e}"),
                Err(e) => warn!("Metrics maintenance task failed: {e}"),
            }
        }
    });
}

/// Persisted metrics store backed by SQLite with a background writer task.
pub struct MetricsStore {
    /// Channel to send writes to the background task.
    write_tx: mpsc::Sender<MetricsWriteEvent>,
    /// Shared connection for reads (load_totals).
    conn: Arc<std::sync::Mutex<Connection>>,
}

impl MetricsStore {
    /// Open or create the metrics database at the given path.
    /// Spawns a background writer task for non-blocking inserts.
    pub fn open(db_path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create metrics dir: {e}"))?;
        }

        let conn = Connection::open(&db_path)
            .map_err(|e| format!("failed to open metrics db at {}: {e}", db_path.display()))?;

        // Enable WAL mode and set busy timeout for better concurrency
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;",
        )
        .map_err(|e| format!("failed to set WAL mode: {e}"))?;

        initialize_metrics_schema(&conn)
            .map_err(|e| format!("failed to initialize metrics schema: {e}"))?;
        if let Err(e) = run_metrics_maintenance(&conn) {
            warn!("Metrics maintenance skipped: {e}");
        }

        info!("Metrics store opened at {}", db_path.display());

        let read_conn = Arc::new(std::sync::Mutex::new(conn));

        // Open a separate connection for the writer task
        let write_conn = Connection::open(&db_path)
            .map_err(|e| format!("failed to open write connection: {e}"))?;
        write_conn
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .map_err(|e| format!("failed to set WAL on writer: {e}"))?;

        let (write_tx, write_rx) = mpsc::channel::<MetricsWriteEvent>(METRICS_WRITE_QUEUE_CAPACITY);

        // Spawn the background writer task
        tokio::spawn(Self::writer_loop(write_conn, write_rx));
        spawn_metrics_maintenance(read_conn.clone());

        Ok(Self {
            write_tx,
            conn: read_conn,
        })
    }

    /// Background task that drains the write channel and batches inserts.
    async fn writer_loop(conn: Connection, mut rx: mpsc::Receiver<MetricsWriteEvent>) {
        let mut conn = Some(conn);
        while let Some(event) = rx.recv().await {
            // Drain any additional buffered events for batching
            let mut batch = vec![event];
            while let Ok(ev) = rx.try_recv() {
                batch.push(ev);
                if batch.len() >= 64 {
                    break;
                }
            }

            // Take the connection out for spawn_blocking
            let c = match conn.take() {
                Some(c) => c,
                None => break,
            };

            // Write batch inside spawn_blocking to avoid blocking the tokio runtime
            let result = tokio::task::spawn_blocking(move || {
                if let Ok(tx) = c.unchecked_transaction() {
                    for ev in &batch {
                        if let Err(e) = Self::insert_metrics_write_event(&tx, ev) {
                            error!("Failed to insert metrics event: {e}");
                        }
                    }
                    if let Err(e) = tx.commit() {
                        error!("Failed to commit usage batch: {e}");
                    }
                } else {
                    error!("Failed to begin transaction");
                }
                c // return the connection after tx is dropped
            })
            .await;

            match result {
                Ok(returned_conn) => {
                    conn = Some(returned_conn);
                }
                Err(e) => {
                    error!("Writer task spawn_blocking panicked: {e}");
                    break;
                }
            }
        }
        info!("Metrics writer task shutting down");
    }

    fn insert_metrics_write_event(
        tx: &rusqlite::Transaction<'_>,
        event: &MetricsWriteEvent,
    ) -> rusqlite::Result<usize> {
        match event {
            MetricsWriteEvent::Usage(ev) => tx.execute(
                "INSERT INTO usage_events (
                    provider, initiator, model, input_tokens, output_tokens,
                    cache_creation_input_tokens, cache_read_input_tokens, is_error, latency_ms,
                    terminal_reason, error_kind
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    ev.provider,
                    ev.initiator,
                    ev.model,
                    ev.input_tokens,
                    ev.output_tokens,
                    ev.cache_creation_input_tokens,
                    ev.cache_read_input_tokens,
                    ev.is_error,
                    ev.latency_ms,
                    ev.terminal_reason,
                    ev.error_kind,
                ],
            ),
            MetricsWriteEvent::Observability(ev) => tx.execute(
            "INSERT INTO request_observability_events (
                request_id, provider, initiator, model, stream, is_error, terminal_reason,
                total_latency_ms, provider_setup_ms, upstream_connect_ms, stream_duration_ms,
                first_event_ms, last_event_gap_ms, max_event_gap_ms, idle_gap_count, event_count,
                transport, websocket_reused, continuation_used, continuation_disabled_reason,
                continuation_fallback_used, fallback_reason, upstream_error_status,
                upstream_error_code, upstream_error_message_class, request_body_bytes,
                upstream_send_body_bytes, continuation_saved_bytes, responses_lite,
                prompt_too_long_retries,
                prompt_too_long_original_body_bytes, prompt_too_long_shrunk_body_bytes,
                prompt_too_long_dropped_items, request_messages, request_content_blocks,
                request_tool_results, request_text_bytes
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37)",
            rusqlite::params![
                ev.request_id,
                ev.provider,
                ev.initiator,
                ev.model,
                ev.stream as i64,
                ev.is_error as i64,
                ev.terminal_reason,
                ev.total_latency_ms as i64,
                ev.provider_setup_ms as i64,
                ev.upstream_connect_ms as i64,
                ev.stream_duration_ms as i64,
                ev.first_event_ms.map(|value| value as i64),
                ev.last_event_gap_ms as i64,
                ev.max_event_gap_ms as i64,
                ev.idle_gap_count as i64,
                ev.event_count as i64,
                ev.transport.as_deref(),
                ev.websocket_reused.map(|value| value as i64),
                ev.continuation_used.map(|value| value as i64),
                ev.continuation_disabled_reason.as_deref(),
                ev.continuation_fallback_used.map(|value| value as i64),
                ev.fallback_reason.as_deref(),
                ev.upstream_error_status.map(|value| value as i64),
                ev.upstream_error_code.as_deref(),
                ev.upstream_error_message_class.as_deref(),
                ev.request_body_bytes as i64,
                ev.upstream_send_body_bytes as i64,
                ev.continuation_saved_bytes as i64,
                ev.responses_lite.map(|value| value as i64),
                ev.prompt_too_long_retries as i64,
                ev.prompt_too_long_original_body_bytes as i64,
                ev.prompt_too_long_shrunk_body_bytes as i64,
                ev.prompt_too_long_dropped_items as i64,
                ev.request_messages as i64,
                ev.request_content_blocks as i64,
                ev.request_tool_results as i64,
                ev.request_text_bytes as i64,
            ],
        ),
    }
    }

    /// Record a completed request with its token usage (non-blocking).
    pub fn record_usage(&self, record: CompletedUsageRecord<'_>) {
        let event = UsageEvent {
            provider: record.provider.to_string(),
            initiator: record.initiator.to_string(),
            model: record.model.to_string(),
            input_tokens: record.usage.input_tokens as i64,
            output_tokens: record.usage.output_tokens as i64,
            cache_creation_input_tokens: record.usage.cache_creation_input_tokens as i64,
            cache_read_input_tokens: record.usage.cache_read_input_tokens as i64,
            is_error: record.is_error as i64,
            latency_ms: record.latency_ms as i64,
            terminal_reason: record.terminal_reason.to_string(),
            error_kind: record.error_kind.to_string(),
        };
        match self.write_tx.try_send(MetricsWriteEvent::Usage(event)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!("Metrics writer channel full, dropping usage event");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!("Metrics writer channel closed, dropping usage event");
            }
        }
    }

    pub fn record_observability(&self, event: RequestObservabilityEvent) {
        match self
            .write_tx
            .try_send(MetricsWriteEvent::Observability(Box::new(event)))
        {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!("Metrics writer channel full, dropping observability event");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!("Metrics writer channel closed, dropping observability event");
            }
        }
    }

    /// Load all-time aggregated totals from the database.
    pub async fn load_totals(&self) -> StoredTotals {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut totals = StoredTotals::default();

            // Global totals
            if let Ok(row) = conn.query_row(
                "SELECT COUNT(*) as requests,
                        COALESCE(SUM(CASE WHEN is_error = 1 THEN 1 ELSE 0 END), 0) as errors,
                        COALESCE(SUM(latency_ms), 0) as latency_sum,
                        COALESCE(SUM(CASE WHEN latency_ms > 0 THEN 1 ELSE 0 END), 0) as latency_count
                 FROM usage_events",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            ) {
                totals.requests_total = row.0 as u64;
                totals.errors_total = row.1 as u64;
                totals.latency_sum_ms = row.2 as u64;
                totals.latency_count = row.3 as u64;
            }

            load_usage_metrics(&conn, "model", &mut totals.model_metrics);
            load_usage_metrics(&conn, "provider", &mut totals.provider_metrics);
            load_usage_metrics(&conn, "initiator", &mut totals.initiator_metrics);
            totals.error_diagnostics = load_error_diagnostics(&conn);

            totals
        })
        .await
        .unwrap_or_default()
    }

    pub async fn load_observability(&self) -> RequestObservabilityStored {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            load_request_observability(&conn)
        })
        .await
        .unwrap_or_default()
    }
}

fn load_request_observability(conn: &Connection) -> RequestObservabilityStored {
    let mut stored = RequestObservabilityStored::default();
    if let Ok(row) = conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN is_error = 1 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(total_latency_ms), 0),
                COALESCE(SUM(upstream_connect_ms), 0),
                COALESCE(MAX(max_event_gap_ms), 0),
                COALESCE(SUM(idle_gap_count), 0),
                COALESCE(SUM(prompt_too_long_retries), 0),
                COALESCE(SUM(continuation_saved_bytes), 0)
         FROM request_observability_events",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        },
    ) {
        stored.summary.requests = row.0 as u64;
        stored.summary.errors = row.1 as u64;
        stored.summary.avg_total_latency_ms = row.2 as u64;
        stored.summary.avg_upstream_connect_ms = row.3 as u64;
        stored.summary.max_event_gap_ms = row.4 as u64;
        stored.summary.idle_gap_count = row.5 as u64;
        stored.summary.prompt_too_long_retries = row.6 as u64;
        stored.summary.continuation_saved_bytes = row.7 as u64;
        stored.summary.finalize();
    }

    if let Ok(mut stmt) = conn.prepare(
        "SELECT request_id, provider, initiator, model, stream, is_error, terminal_reason,
                total_latency_ms, provider_setup_ms, upstream_connect_ms, stream_duration_ms,
                first_event_ms, last_event_gap_ms, max_event_gap_ms, idle_gap_count, event_count,
                transport, websocket_reused, continuation_used, continuation_disabled_reason,
                continuation_fallback_used, fallback_reason, upstream_error_status,
                upstream_error_code, upstream_error_message_class, request_body_bytes,
                upstream_send_body_bytes, continuation_saved_bytes, responses_lite,
                prompt_too_long_retries,
                prompt_too_long_original_body_bytes, prompt_too_long_shrunk_body_bytes,
                prompt_too_long_dropped_items, request_messages, request_content_blocks,
                request_tool_results, request_text_bytes
         FROM request_observability_events
         ORDER BY id DESC
         LIMIT 20",
    ) && let Ok(rows) = stmt.query_map([], request_observability_from_row)
    {
        stored.recent = rows.flatten().collect();
        stored.recent.reverse();
    }

    stored
}

fn request_observability_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<RequestObservabilityEvent> {
    Ok(RequestObservabilityEvent {
        request_id: row.get(0)?,
        provider: row.get(1)?,
        initiator: row.get(2)?,
        model: row.get(3)?,
        stream: row.get::<_, i64>(4)? != 0,
        is_error: row.get::<_, i64>(5)? != 0,
        terminal_reason: row.get(6)?,
        total_latency_ms: row.get::<_, i64>(7)? as u64,
        provider_setup_ms: row.get::<_, i64>(8)? as u64,
        upstream_connect_ms: row.get::<_, i64>(9)? as u64,
        stream_duration_ms: row.get::<_, i64>(10)? as u64,
        first_event_ms: row.get::<_, Option<i64>>(11)?.map(|value| value as u64),
        last_event_gap_ms: row.get::<_, i64>(12)? as u64,
        max_event_gap_ms: row.get::<_, i64>(13)? as u64,
        idle_gap_count: row.get::<_, i64>(14)? as u64,
        event_count: row.get::<_, i64>(15)? as u64,
        transport: row.get(16)?,
        websocket_reused: row.get::<_, Option<i64>>(17)?.map(|value| value != 0),
        continuation_used: row.get::<_, Option<i64>>(18)?.map(|value| value != 0),
        continuation_disabled_reason: row.get(19)?,
        continuation_fallback_used: row.get::<_, Option<i64>>(20)?.map(|value| value != 0),
        fallback_reason: row.get(21)?,
        upstream_error_status: row.get::<_, Option<i64>>(22)?.map(|value| value as u64),
        upstream_error_code: row.get(23)?,
        upstream_error_message_class: row.get(24)?,
        request_body_bytes: row.get::<_, i64>(25)? as u64,
        upstream_send_body_bytes: row.get::<_, i64>(26)? as u64,
        continuation_saved_bytes: row.get::<_, i64>(27)? as u64,
        responses_lite: row.get::<_, Option<i64>>(28)?.map(|value| value != 0),
        prompt_too_long_retries: row.get::<_, i64>(29)? as u64,
        prompt_too_long_original_body_bytes: row.get::<_, i64>(30)? as u64,
        prompt_too_long_shrunk_body_bytes: row.get::<_, i64>(31)? as u64,
        prompt_too_long_dropped_items: row.get::<_, i64>(32)? as u64,
        request_messages: row.get::<_, i64>(33)? as u64,
        request_content_blocks: row.get::<_, i64>(34)? as u64,
        request_tool_results: row.get::<_, i64>(35)? as u64,
        request_text_bytes: row.get::<_, i64>(36)? as u64,
    })
}

fn load_usage_metrics(
    conn: &Connection,
    group_field: &str,
    target: &mut std::collections::HashMap<String, UsageMetrics>,
) {
    let query = format!(
        "SELECT {group_field},
                COUNT(*) as requests,
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_creation_input_tokens), 0),
                COALESCE(SUM(cache_read_input_tokens), 0)
         FROM usage_events
         GROUP BY {group_field}"
    );
    if let Ok(mut stmt) = conn.prepare(&query)
        && let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
    {
        for row in rows.flatten() {
            target.insert(
                row.0,
                UsageMetrics {
                    requests: row.1 as u64,
                    input_tokens: row.2 as u64,
                    output_tokens: row.3 as u64,
                    cache_creation_input_tokens: row.4 as u64,
                    cache_read_input_tokens: row.5 as u64,
                },
            );
        }
    }
}

fn load_error_diagnostics(conn: &Connection) -> ErrorDiagnostics {
    let mut diagnostics = ErrorDiagnostics::default();
    if let Ok(errors) = conn.query_row(
        "SELECT COALESCE(SUM(CASE WHEN is_error = 1 THEN 1 ELSE 0 END), 0) FROM usage_events",
        [],
        |row| row.get::<_, i64>(0),
    ) {
        diagnostics.errors = errors as u64;
    }

    if let Ok(mut stmt) = conn.prepare(
        "SELECT terminal_reason, COUNT(*) FROM usage_events
         WHERE is_error = 1 AND terminal_reason != ''
         GROUP BY terminal_reason",
    ) && let Ok(rows) = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    }) {
        for row in rows.flatten() {
            diagnostics.terminal_reasons.insert(row.0, row.1 as u64);
        }
    }

    if let Ok(mut stmt) = conn.prepare(
        "SELECT error_kind, COUNT(*) FROM usage_events
         WHERE is_error = 1 AND error_kind != ''
         GROUP BY error_kind",
    ) && let Ok(rows) = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    }) {
        for row in rows.flatten() {
            diagnostics.error_kinds.insert(row.0, row.1 as u64);
        }
    }

    diagnostics
}

#[derive(Debug, Clone, Default)]
pub struct StoredTotals {
    pub requests_total: u64,
    pub errors_total: u64,
    pub latency_sum_ms: u64,
    pub latency_count: u64,
    pub model_metrics: std::collections::HashMap<String, UsageMetrics>,
    pub provider_metrics: std::collections::HashMap<String, UsageMetrics>,
    pub initiator_metrics: std::collections::HashMap<String, UsageMetrics>,
    pub error_diagnostics: ErrorDiagnostics,
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_db_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("claude-proxy-{name}-{nanos}.db"))
    }

    #[test]
    fn schema_rebuilds_legacy_usage_events_table() {
        let path = temp_db_path("legacy-schema");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                is_error INTEGER NOT NULL DEFAULT 0,
                latency_ms INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            INSERT INTO usage_events (model, input_tokens) VALUES ('old-model', 10);",
        )
        .unwrap();

        rebuild_usage_events_schema(&conn).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        conn.prepare("SELECT provider, initiator FROM usage_events LIMIT 0")
            .unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn schema_adds_diagnostic_columns_to_existing_usage_events_table() {
        let path = temp_db_path("diagnostic-columns");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                provider TEXT NOT NULL,
                initiator TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                is_error INTEGER NOT NULL DEFAULT 0,
                latency_ms INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            INSERT INTO usage_events (provider, initiator, model, input_tokens)
            VALUES ('chatgpt', 'user', 'gpt-5.5', 10);",
        )
        .unwrap();

        rebuild_usage_events_schema(&conn).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        conn.prepare("SELECT terminal_reason, error_kind FROM usage_events LIMIT 0")
            .unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn prunes_usage_events_older_than_retention_window() {
        let path = temp_db_path("retention");
        let conn = Connection::open(&path).unwrap();
        rebuild_usage_events_schema(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO usage_events (provider, initiator, model, created_at)
             VALUES ('chatgpt', 'user', 'gpt-5.5', datetime('now', '-91 days'));
             INSERT INTO usage_events (provider, initiator, model, created_at)
             VALUES ('chatgpt', 'user', 'gpt-5.5', datetime('now', '-89 days'));",
        )
        .unwrap();

        let deleted = prune_old_usage_events(&conn, 90).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(count, 1);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn load_totals_groups_usage_by_model_provider_and_initiator() {
        let path = temp_db_path("usage-totals");
        let store = MetricsStore::open(path.clone()).unwrap();
        store.record_usage(CompletedUsageRecord {
            provider: "chatgpt",
            initiator: "agent",
            model: "gpt-5.5",
            usage: &TokenUsage {
                input_tokens: 11,
                output_tokens: 7,
                cache_creation_input_tokens: 3,
                cache_read_input_tokens: 2,
            },
            is_error: false,
            latency_ms: 123,
            terminal_reason: "completed",
            error_kind: "",
        });
        store.record_usage(CompletedUsageRecord {
            provider: "openai",
            initiator: "user",
            model: "gpt-4.1",
            usage: &TokenUsage {
                input_tokens: 5,
                output_tokens: 13,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1,
            },
            is_error: true,
            latency_ms: 77,
            terminal_reason: "provider_error",
            error_kind: "rate_limited",
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let totals = store.load_totals().await;

        assert_eq!(totals.requests_total, 2);
        assert_eq!(totals.errors_total, 1);
        assert_eq!(totals.model_metrics["gpt-5.5"].input_tokens, 11);
        assert_eq!(totals.model_metrics["gpt-4.1"].output_tokens, 13);
        assert_eq!(
            totals.provider_metrics["chatgpt"].cache_creation_input_tokens,
            3
        );
        assert_eq!(totals.provider_metrics["openai"].cache_read_input_tokens, 1);
        assert_eq!(totals.initiator_metrics["agent"].requests, 1);
        assert_eq!(totals.initiator_metrics["user"].output_tokens, 13);
        assert_eq!(totals.error_diagnostics.errors, 1);
        assert_eq!(
            totals.error_diagnostics.terminal_reasons["provider_error"],
            1
        );
        assert_eq!(totals.error_diagnostics.error_kinds["rate_limited"], 1);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    fn test_observability_event(
        request_id: &str,
        is_error: bool,
        total_latency_ms: u64,
        upstream_connect_ms: u64,
    ) -> RequestObservabilityEvent {
        RequestObservabilityEvent {
            request_id: request_id.to_string(),
            provider: "chatgpt".to_string(),
            initiator: "user".to_string(),
            model: "gpt-5.5".to_string(),
            stream: true,
            is_error,
            terminal_reason: if is_error {
                "stream_error"
            } else {
                "completed"
            }
            .to_string(),
            total_latency_ms,
            provider_setup_ms: 10,
            upstream_connect_ms,
            stream_duration_ms: total_latency_ms.saturating_sub(upstream_connect_ms),
            first_event_ms: Some(25),
            last_event_gap_ms: 7,
            max_event_gap_ms: 45,
            idle_gap_count: 1,
            event_count: 4,
            transport: Some("websocket".to_string()),
            websocket_reused: Some(true),
            continuation_used: Some(true),
            continuation_disabled_reason: Some("none".to_string()),
            continuation_fallback_used: Some(is_error),
            fallback_reason: is_error.then(|| "previous_response_not_found".to_string()),
            upstream_error_status: is_error.then_some(400),
            upstream_error_code: is_error.then(|| "context_length_exceeded".to_string()),
            upstream_error_message_class: is_error.then(|| "context_length_exceeded".to_string()),
            request_body_bytes: 1_000,
            upstream_send_body_bytes: 120,
            continuation_saved_bytes: 880,
            responses_lite: Some(true),
            prompt_too_long_retries: 1,
            prompt_too_long_original_body_bytes: 200,
            prompt_too_long_shrunk_body_bytes: 120,
            prompt_too_long_dropped_items: 2,
            request_messages: 3,
            request_content_blocks: 5,
            request_tool_results: 1,
            request_text_bytes: 80,
        }
    }

    #[tokio::test]
    async fn load_observability_returns_summary_and_recent_rows() {
        let path = temp_db_path("observability-totals");
        let store = MetricsStore::open(path.clone()).unwrap();
        store.record_observability(test_observability_event("first", false, 100, 20));
        store.record_observability(test_observability_event("second", true, 300, 40));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stored = store.load_observability().await;

        assert_eq!(stored.summary.requests, 2);
        assert_eq!(stored.summary.errors, 1);
        assert_eq!(stored.summary.avg_total_latency_ms, 200);
        assert_eq!(stored.summary.avg_upstream_connect_ms, 30);
        assert_eq!(stored.summary.max_event_gap_ms, 45);
        assert_eq!(stored.summary.idle_gap_count, 2);
        assert_eq!(stored.summary.prompt_too_long_retries, 2);
        assert_eq!(stored.summary.continuation_saved_bytes, 1_760);
        assert_eq!(stored.recent.len(), 2);
        assert_eq!(stored.recent[0].request_id, "first");
        assert_eq!(stored.recent[1].request_id, "second");
        assert_eq!(stored.recent[1].terminal_reason, "stream_error");
        assert_eq!(stored.recent[1].request_tool_results, 1);
        assert_eq!(stored.recent[0].transport.as_deref(), Some("websocket"));
        assert_eq!(stored.recent[0].websocket_reused, Some(true));
        assert_eq!(stored.recent[0].continuation_used, Some(true));
        assert_eq!(
            stored.recent[0].continuation_disabled_reason.as_deref(),
            Some("none")
        );
        assert_eq!(stored.recent[0].request_body_bytes, 1_000);
        assert_eq!(stored.recent[0].upstream_send_body_bytes, 120);
        assert_eq!(stored.recent[0].continuation_saved_bytes, 880);
        assert_eq!(stored.recent[0].responses_lite, Some(true));
        assert_eq!(stored.recent[1].continuation_fallback_used, Some(true));
        assert_eq!(
            stored.recent[1].fallback_reason.as_deref(),
            Some("previous_response_not_found")
        );
        assert_eq!(stored.recent[1].upstream_error_status, Some(400));
        assert_eq!(
            stored.recent[1].upstream_error_code.as_deref(),
            Some("context_length_exceeded")
        );
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn legacy_observability_schema_gets_new_columns_with_defaults() {
        let path = temp_db_path("observability-legacy-schema");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE request_observability_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                request_id TEXT NOT NULL,
                provider TEXT NOT NULL,
                initiator TEXT NOT NULL,
                model TEXT NOT NULL,
                stream INTEGER NOT NULL DEFAULT 0,
                is_error INTEGER NOT NULL DEFAULT 0,
                terminal_reason TEXT NOT NULL,
                total_latency_ms INTEGER NOT NULL DEFAULT 0,
                provider_setup_ms INTEGER NOT NULL DEFAULT 0,
                upstream_connect_ms INTEGER NOT NULL DEFAULT 0,
                stream_duration_ms INTEGER NOT NULL DEFAULT 0,
                first_event_ms INTEGER,
                last_event_gap_ms INTEGER NOT NULL DEFAULT 0,
                max_event_gap_ms INTEGER NOT NULL DEFAULT 0,
                idle_gap_count INTEGER NOT NULL DEFAULT 0,
                event_count INTEGER NOT NULL DEFAULT 0,
                prompt_too_long_retries INTEGER NOT NULL DEFAULT 0,
                prompt_too_long_original_body_bytes INTEGER NOT NULL DEFAULT 0,
                prompt_too_long_shrunk_body_bytes INTEGER NOT NULL DEFAULT 0,
                prompt_too_long_dropped_items INTEGER NOT NULL DEFAULT 0,
                request_messages INTEGER NOT NULL DEFAULT 0,
                request_content_blocks INTEGER NOT NULL DEFAULT 0,
                request_tool_results INTEGER NOT NULL DEFAULT 0,
                request_text_bytes INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            INSERT INTO request_observability_events (
                request_id, provider, initiator, model, stream, is_error, terminal_reason,
                total_latency_ms, upstream_connect_ms, event_count, request_messages
            ) VALUES ('legacy', 'chatgpt', 'user', 'gpt-5.5', 1, 0, 'completed', 42, 7, 2, 1);",
        )
        .unwrap();

        initialize_metrics_schema(&conn).unwrap();

        assert!(request_observability_events_has_column(&conn, "transport").unwrap());
        assert!(request_observability_events_has_column(&conn, "request_body_bytes").unwrap());
        assert!(
            request_observability_events_has_column(&conn, "continuation_saved_bytes").unwrap()
        );
        assert!(request_observability_events_has_column(&conn, "responses_lite").unwrap());
        let stored = load_request_observability(&conn);
        assert_eq!(stored.recent.len(), 1);
        let event = &stored.recent[0];
        assert_eq!(event.request_id, "legacy");
        assert_eq!(event.transport, None);
        assert_eq!(event.websocket_reused, None);
        assert_eq!(event.request_body_bytes, 0);
        assert_eq!(event.upstream_send_body_bytes, 0);
        assert_eq!(event.continuation_saved_bytes, 0);
        assert_eq!(event.responses_lite, None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn prunes_request_observability_events_older_than_retention_window() {
        let path = temp_db_path("observability-retention");
        let conn = Connection::open(&path).unwrap();
        rebuild_request_observability_schema(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO request_observability_events (request_id, provider, initiator, model, terminal_reason, created_at)
             VALUES ('old', 'chatgpt', 'user', 'gpt-5.5', 'completed', datetime('now', '-91 days'));
             INSERT INTO request_observability_events (request_id, provider, initiator, model, terminal_reason, created_at)
             VALUES ('new', 'chatgpt', 'user', 'gpt-5.5', 'completed', datetime('now', '-89 days'));",
        )
        .unwrap();

        let deleted = prune_old_request_observability_events(&conn, 90).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM request_observability_events",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(count, 1);
        let _ = std::fs::remove_file(path);
    }
}
