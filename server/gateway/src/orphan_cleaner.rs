use std::path::PathBuf;
use std::time::Duration;

use metrics::counter;
use sqlx::PgPool;
use tokio::fs;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Grace period: files younger than this are skipped to avoid racing with
/// in-flight uploads that haven't inserted their DB row yet.
const DEFAULT_GRACE_SECS: u64 = 300; // 5 minutes

/// Walk the upload directory, find files with no matching `attachments` row,
/// and delete them. Runs on a periodic interval.
pub async fn run_orphan_cleaner(pool: PgPool, upload_dir: PathBuf, interval: Duration) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Skip the first immediate tick so the server can finish starting up.
    tick.tick().await;

    loop {
        tick.tick().await;
        match scan_and_remove(&pool, &upload_dir).await {
            Ok((scanned, removed)) => {
                if removed > 0 {
                    info!(scanned, removed, "orphan upload cleaner pass complete");
                } else {
                    debug!(scanned, "orphan upload cleaner pass complete, no orphans");
                }
            }
            Err(e) => {
                warn!(error = %e, "orphan upload cleaner encountered an error");
            }
        }
    }
}

async fn scan_and_remove(pool: &PgPool, upload_dir: &PathBuf) -> anyhow::Result<(u64, u64)> {
    let mut scanned: u64 = 0;
    let mut removed: u64 = 0;

    let mut shard_entries = fs::read_dir(upload_dir).await?;
    while let Some(shard_entry) = shard_entries.next_entry().await? {
        let shard_path = shard_entry.path();
        if !shard_path.is_dir() {
            continue;
        }

        let mut file_entries = fs::read_dir(&shard_path).await?;
        while let Some(file_entry) = file_entries.next_entry().await? {
            let file_path = file_entry.path();
            if !file_path.is_file() {
                continue;
            }

            scanned += 1;

            // The filename is the attachment UUID.
            let file_name = match file_path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            let id = match Uuid::parse_str(&file_name) {
                Ok(id) => id,
                Err(_) => continue, // skip non-UUID files
            };

            // Check file age — skip files within the grace period.
            if let Ok(meta) = file_entry.metadata().await {
                if let Ok(modified) = meta.modified() {
                    if let Ok(age) = modified.elapsed() {
                        if age < Duration::from_secs(DEFAULT_GRACE_SECS) {
                            continue;
                        }
                    }
                }
            }

            // Check if a corresponding DB row exists.
            let exists = sqlx::query_scalar::<_, i64>(
                "SELECT 1 FROM attachments WHERE id = $1 LIMIT 1",
            )
            .bind(id)
            .fetch_optional(pool)
            .await;

            match exists {
                Ok(Some(_)) => {} // row exists, file is referenced
                Ok(None) => {
                    // Orphan — delete.
                    if let Err(e) = fs::remove_file(&file_path).await {
                        warn!(path = %file_path.display(), error = %e, "failed to remove orphan upload file");
                    } else {
                        debug!(path = %file_path.display(), "removed orphan upload file");
                        removed += 1;
                        counter!("vp_orphan_uploads_removed_total").increment(1);
                    }
                }
                Err(e) => {
                    warn!(id = %id, error = %e, "DB check failed during orphan scan, skipping file");
                }
            }
        }
    }

    counter!("vp_orphan_uploads_scanned_total").increment(scanned);
    Ok((scanned, removed))
}
