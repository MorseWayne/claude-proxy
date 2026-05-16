use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::app::TokenUsage;

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
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_usage_events_provider ON usage_events(provider);
        CREATE INDEX IF NOT EXISTS idx_usage_events_initiator ON usage_events(initiator);
        CREATE INDEX IF NOT EXISTS idx_usage_events_model ON usage_events(model);
        CREATE INDEX IF NOT EXISTS idx_usage_events_created ON usage_events(created_at);",
    )
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

        rebuild_usage_events_schema(&conn)
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
                            "INSERT INTO usage_events (provider, initiator, model, input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens, is_error, latency_ms)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
    pub fn record_usage(
        &self,
        provider: &str,
        initiator: &str,
        model: &str,
        usage: &TokenUsage,
        is_error: bool,
        latency_ms: u64,
    ) {
        let event = UsageEvent {
            provider: provider.to_string(),
            initiator: initiator.to_string(),
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

    #[tokio::test]
    async fn record_usage_persists_provider_and_initiator() {
        let path = temp_db_path("usage-event");
        let store = MetricsStore::open(path.clone()).unwrap();
        store.record_usage(
            "chatgpt",
            "agent",
            "gpt-5.5",
            &TokenUsage {
                input_tokens: 11,
                output_tokens: 7,
                cache_creation_input_tokens: 3,
                cache_read_input_tokens: 2,
            },
            false,
            123,
        );
        drop(store);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let conn = Connection::open(&path).unwrap();
        let row: (String, String, String, i64, i64) = conn
            .query_row(
                "SELECT provider, initiator, model, input_tokens, output_tokens FROM usage_events",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(
            row,
            ("chatgpt".into(), "agent".into(), "gpt-5.5".into(), 11, 7)
        );
        let _ = std::fs::remove_file(path);
    }
}
