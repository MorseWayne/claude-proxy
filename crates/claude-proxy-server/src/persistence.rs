use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::app::TokenUsage;

/// A single write event to be persisted.
struct UsageEvent {
    model: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_input_tokens: i64,
    cache_read_input_tokens: i64,
    is_error: i64,
    latency_ms: i64,
}

/// Persisted metrics store backed by SQLite with a background writer task.
pub struct MetricsStore {
    /// Channel to send writes to the background task.
    write_tx: mpsc::UnboundedSender<UsageEvent>,
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

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage_events (
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
            CREATE INDEX IF NOT EXISTS idx_usage_events_model ON usage_events(model);
            CREATE INDEX IF NOT EXISTS idx_usage_events_created ON usage_events(created_at);",
        )
        .map_err(|e| format!("failed to initialize metrics schema: {e}"))?;

        info!("Metrics store opened at {}", db_path.display());

        let read_conn = Arc::new(std::sync::Mutex::new(conn));

        // Open a separate connection for the writer task
        let write_conn = Connection::open(&db_path)
            .map_err(|e| format!("failed to open write connection: {e}"))?;
        write_conn
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .map_err(|e| format!("failed to set WAL on writer: {e}"))?;

        let (write_tx, write_rx) = mpsc::unbounded_channel::<UsageEvent>();

        // Spawn the background writer task
        tokio::spawn(Self::writer_loop(write_conn, write_rx));

        Ok(Self {
            write_tx,
            conn: read_conn,
        })
    }

    /// Background task that drains the write channel and batches inserts.
    async fn writer_loop(conn: Connection, mut rx: mpsc::UnboundedReceiver<UsageEvent>) {
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
                        if let Err(e) = tx.execute(
                            "INSERT INTO usage_events (model, input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens, is_error, latency_ms)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                            rusqlite::params![
                                ev.model,
                                ev.input_tokens,
                                ev.output_tokens,
                                ev.cache_creation_input_tokens,
                                ev.cache_read_input_tokens,
                                ev.is_error,
                                ev.latency_ms,
                            ],
                        ) {
                            error!("Failed to insert usage event: {e}");
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

    /// Record a completed request with its token usage (non-blocking).
    pub fn record_usage(&self, model: &str, usage: &TokenUsage, is_error: bool, latency_ms: u64) {
        let event = UsageEvent {
            model: model.to_string(),
            input_tokens: usage.input_tokens as i64,
            output_tokens: usage.output_tokens as i64,
            cache_creation_input_tokens: usage.cache_creation_input_tokens as i64,
            cache_read_input_tokens: usage.cache_read_input_tokens as i64,
            is_error: is_error as i64,
            latency_ms: latency_ms as i64,
        };
        if self.write_tx.send(event).is_err() {
            warn!("Metrics writer channel closed, dropping usage event");
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

            // Per-model totals
            if let Ok(mut stmt) = conn.prepare(
                "SELECT model,
                        COUNT(*) as requests,
                        COALESCE(SUM(input_tokens), 0),
                        COALESCE(SUM(output_tokens), 0),
                        COALESCE(SUM(cache_creation_input_tokens), 0),
                        COALESCE(SUM(cache_read_input_tokens), 0)
                 FROM usage_events
                 GROUP BY model",
            ) && let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            }) {
                for row in rows.flatten() {
                    totals.model_metrics.insert(
                        row.0,
                        StoredModelMetrics {
                            requests: row.1 as u64,
                            input_tokens: row.2 as u64,
                            output_tokens: row.3 as u64,
                            cache_creation_input_tokens: row.4 as u64,
                            cache_read_input_tokens: row.5 as u64,
                        },
                    );
                }
            }

            totals
        })
        .await
        .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default)]
pub struct StoredTotals {
    pub requests_total: u64,
    pub errors_total: u64,
    pub latency_sum_ms: u64,
    pub latency_count: u64,
    pub model_metrics: std::collections::HashMap<String, StoredModelMetrics>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StoredModelMetrics {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}
