use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::app::TokenUsage;

/// Persisted metrics store backed by SQLite.
pub struct MetricsStore {
    conn: Arc<Mutex<Connection>>,
}

impl MetricsStore {
    /// Open or create the metrics database at the given path.
    pub fn open(db_path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create metrics dir: {e}"))?;
        }

        let conn = Connection::open(&db_path)
            .map_err(|e| format!("failed to open metrics db at {}: {e}", db_path.display()))?;

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
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Record a completed request with its token usage.
    pub async fn record_usage(
        &self,
        model: &str,
        usage: &TokenUsage,
        is_error: bool,
        latency_ms: u64,
    ) {
        let conn = self.conn.lock().await;
        if let Err(e) = conn.execute(
            "INSERT INTO usage_events (model, input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens, is_error, latency_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                model,
                usage.input_tokens as i64,
                usage.output_tokens as i64,
                usage.cache_creation_input_tokens as i64,
                usage.cache_read_input_tokens as i64,
                is_error as i64,
                latency_ms as i64,
            ],
        ) {
            error!("Failed to persist usage event: {e}");
        }
    }

    /// Load all-time aggregated totals from the database.
    pub async fn load_totals(&self) -> StoredTotals {
        let conn = self.conn.lock().await;
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
        )
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
