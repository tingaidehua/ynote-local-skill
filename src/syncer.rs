use crate::cloud::{self, CloudStats};
use crate::exporter;
use crate::mirror::{self, SyncRecord};
use crate::repository::Repository;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_INTERVAL_SECONDS: u64 = 15 * 60;
pub const MIN_INTERVAL_SECONDS: u64 = 5 * 60;
pub const DEFAULT_JITTER_SECONDS: u64 = 2 * 60;
pub const MAX_BACKOFF_SECONDS: u64 = 2 * 60 * 60;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSummary {
    pub output: PathBuf,
    pub database: PathBuf,
    pub backend: String,
    pub started_at_unix: u64,
    pub finished_at_unix: u64,
    pub notes: usize,
    pub resources: usize,
    pub external_edits_captured: usize,
    pub warnings: Vec<String>,
    pub cloud: Option<CloudStats>,
}

pub struct SyncOutcome {
    pub repo: Repository,
    pub summary: SyncSummary,
}

pub fn refresh_once(
    data_root: Option<PathBuf>,
    account: Option<String>,
    output: &Path,
    local_only: bool,
) -> Result<SyncOutcome> {
    let _lock = SyncLock::acquire(output)?;
    let started_at_unix = unix_now();
    let external_edits_captured = mirror::capture_external_edits(output)?;
    let local = Repository::discover(data_root, account)?;
    let (repo, cloud_stats, backend) = if local_only {
        (local, None, "desktop_local".to_string())
    } else {
        let (repo, stats) = cloud::refresh(local, output)?;
        (repo, Some(stats), "youdao_cloud_readonly".to_string())
    };
    let manifest = exporter::export(&repo, output)?;
    let finished_at_unix = unix_now();
    let stats = json!({
        "notes": manifest.note_count,
        "resources": manifest.resource_count,
        "externalEditsCaptured": external_edits_captured,
        "warnings": manifest.warnings.clone(),
        "cloud": cloud_stats.clone()
    });
    let record = SyncRecord {
        started_at_unix,
        finished_at_unix,
        backend: backend.clone(),
        success: true,
        message: "sync completed".to_string(),
        stats,
    };
    let database = mirror::write_snapshot(&repo, output, &record)?;
    let live_repo = mirror::load(&database)?;
    Ok(SyncOutcome {
        repo: live_repo,
        summary: SyncSummary {
            output: output.to_path_buf(),
            database,
            backend,
            started_at_unix,
            finished_at_unix,
            notes: manifest.note_count,
            resources: manifest.resource_count,
            external_edits_captured,
            warnings: manifest.warnings,
            cloud: cloud_stats,
        },
    })
}

pub fn validate_interval(interval: u64) -> Result<u64> {
    if interval < MIN_INTERVAL_SECONDS {
        bail!(
            "sync interval {interval}s is too frequent; minimum is {MIN_INTERVAL_SECONDS}s to reduce account and service risk"
        );
    }
    Ok(interval)
}

pub fn next_delay(interval: u64, jitter: u64, failures: u32) -> Duration {
    let backoff = if failures == 0 {
        interval
    } else {
        interval
            .saturating_mul(1u64 << failures.min(4))
            .min(MAX_BACKOFF_SECONDS)
    };
    let spread = if jitter == 0 {
        0
    } else {
        (unix_now() ^ u64::from(std::process::id())) % (jitter + 1)
    };
    Duration::from_secs(backoff.saturating_add(spread))
}

struct SyncLock {
    path: PathBuf,
}

impl SyncLock {
    fn acquire(output: &Path) -> Result<Self> {
        let internal = output.join("_ynote");
        fs::create_dir_all(&internal)?;
        let path = internal.join("sync.lock");
        let open = || OpenOptions::new().write(true).create_new(true).open(&path);
        let mut file = match open() {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                    .is_some_and(|age| age > Duration::from_secs(MAX_BACKOFF_SECONDS + 300));
                if !stale {
                    bail!("another ynote-cli synchronization is already running");
                }
                fs::remove_file(&path).context("remove stale synchronization lock")?;
                open().context("create synchronization lock after stale-lock cleanup")?
            }
            Err(error) => return Err(error).context("create synchronization lock"),
        };
        writeln!(file, "pid={}\nstarted={}", std::process::id(), unix_now())?;
        file.sync_all()?;
        Ok(Self { path })
    }
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::{MAX_BACKOFF_SECONDS, MIN_INTERVAL_SECONDS, next_delay, validate_interval};

    #[test]
    fn enforces_safe_polling_floor_and_backoff_cap() {
        assert!(validate_interval(MIN_INTERVAL_SECONDS - 1).is_err());
        assert_eq!(
            validate_interval(MIN_INTERVAL_SECONDS).unwrap(),
            MIN_INTERVAL_SECONDS
        );
        assert!(next_delay(MIN_INTERVAL_SECONDS, 0, 99).as_secs() <= MAX_BACKOFF_SECONDS);
    }
}
