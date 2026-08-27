use std::path::{Path, PathBuf};

use crate::core::model::DatasetLogEntry;
use crate::core::state::DatasetLogSender;

/// `provider_id` is admin/operator-controlled and, on the export/import
/// path, not even validated by `core::error::validate_path_id` (which only
/// rejects empty strings and `/`) - never join it onto a filesystem path
/// unsanitized. Keeps `[A-Za-z0-9._-]` only, collapses anything else to
/// `_`, and refuses `.`/`..` outright.
fn sanitize_path_component(raw: &str) -> String {
    if raw.is_empty() || raw == "." || raw == ".." {
        return "_invalid".to_string();
    }
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Spawns the background writer and returns a bounded sender. Mirrors
/// `telemetry::request_log::spawn_writer`'s shape (bounded channel +
/// background task, never blocks the hot path - callers use `try_send`),
/// but appends JSONL lines under `dir` instead of inserting SQL rows.
/// Best-effort: a write failure logs a warning and drops that entry rather
/// than panicking the writer task.
pub fn spawn_writer(dir: PathBuf, buffer: usize) -> DatasetLogSender {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<DatasetLogEntry>(buffer);

    tokio::spawn(async move {
        while let Some(entry) = rx.recv().await {
            if let Err(e) = write_entry(&dir, &entry).await {
                tracing::warn!(error = %e, "dataset_log: failed to write entry, dropping");
            }
        }
    });

    tx
}

async fn write_entry(dir: &Path, entry: &DatasetLogEntry) -> std::io::Result<()> {
    let safe_id = sanitize_path_component(&entry.provider_id);
    let provider_dir = dir.join(&safe_id);
    tokio::fs::create_dir_all(&provider_dir).await?;

    let file_path = provider_dir.join(format!("{}.jsonl", entry.timestamp.format("%Y-%m-%d")));
    let mut line = serde_json::to_string(entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');

    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&file_path)
        .await?;
    file.write_all(line.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::LatencyMs;
    use crate::core::model::WireFormat;
    use chrono::Utc;

    fn entry(provider_id: &str) -> DatasetLogEntry {
        DatasetLogEntry {
            request_id: uuid::Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            pool_id: Some("pool1".into()),
            provider_id: provider_id.into(),
            model: "m".into(),
            user_id: None,
            wire_format: WireFormat::OpenAi,
            stream: false,
            input_body: "{\"messages\":[]}".into(),
            output_body: "{\"choices\":[]}".into(),
            complete: true,
            latency_ms: LatencyMs { ttfb_ms: Some(10), total_ms: 20 },
        }
    }

    #[tokio::test]
    async fn writer_appends_one_jsonl_line_per_entry() {
        let dir = tempfile::tempdir().unwrap();
        let tx = spawn_writer(dir.path().to_path_buf(), 64);

        let e1 = entry("p1");
        let e2 = entry("p1");
        tx.send(e1.clone()).await.unwrap();
        tx.send(e2.clone()).await.unwrap();
        drop(tx);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let file_path = dir
            .path()
            .join("p1")
            .join(format!("{}.jsonl", e1.timestamp.format("%Y-%m-%d")));
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["provider_id"], "p1");
        assert_eq!(parsed["complete"], true);
        assert_eq!(parsed["latency_ms"]["ttfb_ms"], 10);
        assert_eq!(parsed["latency_ms"]["total_ms"], 20);
    }

    #[tokio::test]
    async fn writer_partitions_by_provider_id() {
        let dir = tempfile::tempdir().unwrap();
        let tx = spawn_writer(dir.path().to_path_buf(), 64);
        tx.send(entry("p1")).await.unwrap();
        tx.send(entry("p2")).await.unwrap();
        drop(tx);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert!(dir.path().join("p1").is_dir());
        assert!(dir.path().join("p2").is_dir());
    }

    #[tokio::test]
    async fn writer_creates_missing_directories() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("does").join("not").join("exist");
        let tx = spawn_writer(nested.clone(), 64);
        tx.send(entry("p1")).await.unwrap();
        drop(tx);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert!(nested.join("p1").is_dir());
    }

    #[tokio::test]
    async fn writer_sanitizes_a_hostile_provider_id_instead_of_escaping_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let tx = spawn_writer(dir.path().to_path_buf(), 64);
        tx.send(entry("../../evil")).await.unwrap();
        drop(tx);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Nothing escaped `dir`: walk it and confirm every entry's
        // canonicalized path is still confined under `dir`.
        let canonical_dir = dir.path().canonicalize().unwrap();
        let mut stack = vec![dir.path().to_path_buf()];
        let mut saw_any_file = false;
        while let Some(d) = stack.pop() {
            let mut rd = tokio::fs::read_dir(&d).await.unwrap();
            while let Some(child) = rd.next_entry().await.unwrap() {
                let p = child.path();
                assert!(
                    p.canonicalize().unwrap().starts_with(&canonical_dir),
                    "escaped the dataset-log directory: {p:?}"
                );
                if p.is_dir() {
                    stack.push(p);
                } else {
                    saw_any_file = true;
                }
            }
        }
        assert!(saw_any_file, "expected the sanitized entry to still be written somewhere under dir");
    }

    #[tokio::test]
    async fn send_never_blocks_when_the_channel_is_full() {
        let dir = tempfile::tempdir().unwrap();
        let tx = spawn_writer(dir.path().to_path_buf(), 1);
        // Fire many sends via try_send - none of these await, so a hang
        // here would mean this test itself never completes rather than
        // failing cleanly; that's an acceptable trade-off for a directness
        // guarantee this specific test exists to prove.
        for _ in 0..50 {
            let _ = tx.try_send(entry("p1"));
        }
    }
}
