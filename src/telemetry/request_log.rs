use chrono::Utc;
use sqlx::SqlitePool;

use crate::core::model::LogEntry;
use crate::core::state::RequestLogSender;

pub fn spawn_writer(db: SqlitePool, buffer: usize, batch: usize) -> RequestLogSender {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<LogEntry>(buffer);

    tokio::spawn(async move {
        let mut pending: Vec<LogEntry> = Vec::with_capacity(batch);
        loop {
            let got = rx.recv().await;
            match got {
                Some(entry) => {
                    pending.push(entry);
                    // opportunistically drain more without awaiting
                    while pending.len() < batch {
                        match rx.try_recv() {
                            Ok(e) => pending.push(e),
                            Err(_) => break,
                        }
                    }
                    flush(&db, &mut pending).await;
                }
                None => {
                    // channel closed: final flush and exit
                    flush(&db, &mut pending).await;
                    break;
                }
            }
        }
    });

    tx
}

async fn flush(db: &SqlitePool, pending: &mut Vec<LogEntry>) {
    if pending.is_empty() {
        return;
    }
    let now = Utc::now();
    let mut tx = match db.begin().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "request_log: failed to begin tx, dropping batch");
            pending.clear();
            return;
        }
    };
    for e in pending.iter() {
        let _ = sqlx::query(
            "INSERT INTO request_log (pool_id, provider_id, status_code, latency_ms, success, created_at)
             VALUES (?,?,?,?,?,?)",
        )
        .bind(&e.pool_id)
        .bind(&e.provider_id)
        .bind(e.status_code)
        .bind(e.latency_ms)
        .bind(e.success)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|err| tracing::warn!(error = %err, "request_log: insert failed"));
    }
    if let Err(e) = tx.commit().await {
        tracing::warn!(error = %e, "request_log: commit failed");
    }
    pending.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::init_pool;
    use crate::core::model::LogEntry;

    #[tokio::test]
    async fn writer_persists_entries() {
        let db = init_pool(":memory:").await.unwrap();
        let tx = spawn_writer(db.clone(), 64, 10);

        for i in 0..5 {
            tx.send(LogEntry {
                pool_id: Some("gpt-4o".into()),
                provider_id: Some(format!("p{i}")),
                status_code: Some(200),
                latency_ms: 12,
                success: true,
            })
            .await
            .unwrap();
        }
        drop(tx); // closes channel; writer flushes and exits

        // give the writer a moment to flush
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let n: (i64,) = sqlx::query_as("SELECT count(*) FROM request_log")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n.0, 5);
    }
}
