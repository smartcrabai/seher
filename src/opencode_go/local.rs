use super::auth::OpencodeGoAuth;
use super::types::{OpencodeGoUsageSnapshot, OpencodeGoUsageSource, OpencodeGoUsageWindow};
use chrono::{DateTime, Duration, Utc};
use rusqlite::Connection;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use thiserror::Error;

const FIVE_HOUR_LIMIT_USD: f64 = 12.0;
const WEEKLY_LIMIT_USD: f64 = 30.0;
const MONTHLY_LIMIT_USD: f64 = 60.0;

const WINDOW_SPECS: [WindowSpec; 3] = [
    WindowSpec {
        entry_type: "five_hour_spend",
        window_seconds: 5 * 60 * 60,
        limit_usd: FIVE_HOUR_LIMIT_USD,
    },
    WindowSpec {
        entry_type: "weekly_spend",
        window_seconds: 7 * 24 * 60 * 60,
        limit_usd: WEEKLY_LIMIT_USD,
    },
    WindowSpec {
        entry_type: "monthly_spend",
        window_seconds: 30 * 24 * 60 * 60,
        limit_usd: MONTHLY_LIMIT_USD,
    },
];

const LIMIT_EPSILON: f64 = 1e-9;

#[derive(Debug, Error)]
pub enum OpencodeGoUsageError {
    #[error("could not determine home directory for opencode.db")]
    HomeDirNotFound,

    #[error("failed to read OpenCode usage database: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to query OpenCode usage database: {0}")]
    Sql(#[from] rusqlite::Error),

    #[error("failed to parse OpenCode message row: {0}")]
    Parse(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq)]
struct UsageRecord {
    completed_at: DateTime<Utc>,
    cost_usd: f64,
}

#[derive(Debug, Clone, Copy)]
struct WindowSpec {
    entry_type: &'static str,
    window_seconds: i64,
    limit_usd: f64,
}

#[derive(Debug, Deserialize)]
struct MessageRow {
    role: String,
    #[serde(rename = "providerID")]
    provider_id: Option<String>,
    cost: Option<f64>,
    time: Option<MessageTime>,
}

#[derive(Debug, Deserialize)]
struct MessageTime {
    completed: Option<i64>,
}

pub struct OpencodeGoUsageStore;

impl OpencodeGoUsageStore {
    /// # Errors
    ///
    /// Returns an error when the local `SQLite` history cannot be copied, read,
    /// or parsed.
    pub fn fetch_usage() -> Result<OpencodeGoUsageSnapshot, OpencodeGoUsageError> {
        Self::fetch_usage_with_paths_at(None, None, Utc::now())
    }

    /// # Errors
    ///
    /// Returns an error when the local `SQLite` history cannot be copied, read,
    /// or parsed.
    pub fn fetch_usage_from_path_at(
        db_path: &Path,
        now: DateTime<Utc>,
    ) -> Result<OpencodeGoUsageSnapshot, OpencodeGoUsageError> {
        Self::fetch_usage_with_paths_at(Some(db_path), None, now)
    }

    /// # Errors
    ///
    /// Returns an error when the local `SQLite` history cannot be copied, read,
    /// or parsed.
    pub fn fetch_usage_with_paths_at(
        db_path: Option<&Path>,
        auth_path: Option<&Path>,
        now: DateTime<Utc>,
    ) -> Result<OpencodeGoUsageSnapshot, OpencodeGoUsageError> {
        let credentials_available = auth_path
            .map_or_else(
                OpencodeGoAuth::read_api_key,
                OpencodeGoAuth::read_api_key_from,
            )
            .is_ok();
        let db_path = match db_path {
            Some(path) => path.to_path_buf(),
            None => Self::default_db_path()?,
        };
        let records = if db_path.exists() {
            Self::load_records(&db_path)?
        } else {
            Vec::new()
        };

        Ok(Self::snapshot_from_records(
            now,
            &records,
            credentials_available,
        ))
    }

    fn default_db_path() -> Result<PathBuf, OpencodeGoUsageError> {
        let home = dirs::home_dir().ok_or(OpencodeGoUsageError::HomeDirNotFound)?;
        Ok(home.join(".local/share/opencode/opencode.db"))
    }

    fn load_records(db_path: &Path) -> Result<Vec<UsageRecord>, OpencodeGoUsageError> {
        let (temp_dir, temp_db_path) = Self::copy_sqlite_database(db_path)?;
        let conn = Connection::open(&temp_db_path)?;
        let records = Self::query_records(&conn)?;
        drop(conn);
        drop(temp_dir);
        Ok(records)
    }

    fn copy_sqlite_database(db_path: &Path) -> Result<(TempDir, PathBuf), OpencodeGoUsageError> {
        let temp_dir = tempfile::tempdir()?;
        let file_name = db_path
            .file_name()
            .ok_or_else(|| std::io::Error::other("opencode.db path has no file name"))?;
        let temp_db_path = temp_dir.path().join(file_name);
        std::fs::copy(db_path, &temp_db_path)?;

        for suffix in ["-wal", "-shm"] {
            let sidecar_name = format!("{}{}", file_name.to_string_lossy(), suffix);
            let src = db_path.with_file_name(&sidecar_name);
            if src.exists() {
                let dst = temp_dir.path().join(sidecar_name);
                std::fs::copy(src, dst)?;
            }
        }

        Ok((temp_dir, temp_db_path))
    }

    fn query_records(conn: &Connection) -> Result<Vec<UsageRecord>, OpencodeGoUsageError> {
        let mut stmt = match conn.prepare("SELECT data FROM message") {
            Ok(stmt) => stmt,
            Err(rusqlite::Error::SqliteFailure(_, Some(message)))
                if message.contains("no such table: message") =>
            {
                return Ok(Vec::new());
            }
            Err(err) => return Err(err.into()),
        };
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut records = Vec::new();

        for row in rows {
            let row = serde_json::from_str::<MessageRow>(&row?)?;
            if row.role != "assistant" || row.provider_id.as_deref() != Some("opencode-go") {
                continue;
            }

            let Some(cost_usd) = row.cost else {
                continue;
            };
            if cost_usd <= 0.0 {
                continue;
            }

            let completed = row.time.and_then(|time| time.completed);
            let Some(completed_at) = completed.and_then(DateTime::from_timestamp_millis) else {
                continue;
            };

            records.push(UsageRecord {
                completed_at,
                cost_usd,
            });
        }

        records.sort_by_key(|record| record.completed_at);
        Ok(records)
    }

    fn snapshot_from_records(
        now: DateTime<Utc>,
        records: &[UsageRecord],
        credentials_available: bool,
    ) -> OpencodeGoUsageSnapshot {
        let windows = WINDOW_SPECS
            .iter()
            .map(|spec| Self::window_from_records(now, records, *spec))
            .collect();

        OpencodeGoUsageSnapshot {
            source: OpencodeGoUsageSource::LocalDatabase,
            credentials_available,
            total_messages: records.len(),
            windows,
        }
    }

    fn window_from_records(
        now: DateTime<Utc>,
        records: &[UsageRecord],
        spec: WindowSpec,
    ) -> OpencodeGoUsageWindow {
        let duration = Duration::seconds(spec.window_seconds);
        let window_start = now - duration;
        let active_records: Vec<&UsageRecord> = records
            .iter()
            .filter(|record| record.completed_at >= window_start)
            .collect();
        let spent_usd = active_records
            .iter()
            .map(|record| record.cost_usd)
            .sum::<f64>();

        let resets_at = if spent_usd + LIMIT_EPSILON >= spec.limit_usd {
            let mut remaining = spent_usd;
            active_records.iter().find_map(|record| {
                remaining -= record.cost_usd;
                if remaining + LIMIT_EPSILON < spec.limit_usd {
                    Some(record.completed_at + duration)
                } else {
                    None
                }
            })
        } else {
            None
        };

        OpencodeGoUsageWindow {
            entry_type: spec.entry_type,
            spent_usd,
            limit_usd: spec.limit_usd,
            resets_at,
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn usage_record(ts: i64, cost_usd: f64) -> UsageRecord {
        UsageRecord {
            completed_at: Utc.timestamp_millis_opt(ts).single().unwrap(),
            cost_usd,
        }
    }

    #[test]
    fn snapshot_is_empty_when_no_messages_exist() {
        let now = Utc.timestamp_millis_opt(1_000_000).single().unwrap();
        let snapshot = OpencodeGoUsageStore::snapshot_from_records(now, &[], false);

        assert_eq!(snapshot.total_messages, 0);
        assert_eq!(snapshot.windows.len(), 3);
        assert!(snapshot.windows.iter().all(|window| !window.is_limited()));
        assert!(
            snapshot
                .windows
                .iter()
                .all(|window| window.spent_usd == 0.0)
        );
    }

    #[test]
    fn computes_five_hour_reset_from_oldest_blocking_message() {
        let now = Utc
            .timestamp_millis_opt(20 * 60 * 60 * 1000)
            .single()
            .unwrap();
        let records = vec![
            usage_record(15 * 60 * 60 * 1000, 4.0),
            usage_record(16 * 60 * 60 * 1000, 5.0),
            usage_record(19 * 60 * 60 * 1000, 4.0),
        ];

        let snapshot = OpencodeGoUsageStore::snapshot_from_records(now, &records, true);
        let five_hour = snapshot
            .windows
            .iter()
            .find(|window| window.entry_type == "five_hour_spend")
            .unwrap();

        assert!(five_hour.is_limited());
        assert_eq!(
            five_hour.resets_at,
            Some(records[0].completed_at + Duration::hours(5))
        );
        assert!((five_hour.spent_usd - 13.0).abs() < LIMIT_EPSILON);
    }

    #[test]
    fn computes_longer_windows_independently() {
        let now = Utc
            .timestamp_millis_opt(40 * 24 * 60 * 60 * 1000)
            .single()
            .unwrap();
        let records = vec![
            usage_record(10 * 24 * 60 * 60 * 1000, 31.0),
            usage_record(34 * 24 * 60 * 60 * 1000, 11.0),
            usage_record(35 * 24 * 60 * 60 * 1000, 10.0),
            usage_record(39 * 24 * 60 * 60 * 1000, 10.0),
        ];

        let snapshot = OpencodeGoUsageStore::snapshot_from_records(now, &records, true);
        let weekly = snapshot
            .windows
            .iter()
            .find(|window| window.entry_type == "weekly_spend")
            .unwrap();
        let monthly = snapshot
            .windows
            .iter()
            .find(|window| window.entry_type == "monthly_spend")
            .unwrap();

        assert!(weekly.is_limited());
        assert_eq!(
            weekly.resets_at,
            Some(records[1].completed_at + Duration::days(7))
        );
        assert!(monthly.is_limited());
        assert_eq!(
            monthly.resets_at,
            Some(records[0].completed_at + Duration::days(30))
        );
    }

    #[test]
    fn reads_only_opencode_go_assistant_messages_from_sqlite() -> TestResult {
        let tmp = tempfile::NamedTempFile::new()?;
        let conn = Connection::open(tmp.path())?;
        conn.execute("CREATE TABLE message (data TEXT NOT NULL)", [])?;
        conn.execute(
            "INSERT INTO message (data) VALUES (?1)",
            [r#"{"role":"assistant","providerID":"opencode-go","cost":1.5,"time":{"completed":3600000}}"#],
        )?;
        conn.execute(
            "INSERT INTO message (data) VALUES (?1)",
            [r#"{"role":"assistant","providerID":"opencode","cost":9.0,"time":{"completed":3600000}}"#],
        )?;
        conn.execute(
            "INSERT INTO message (data) VALUES (?1)",
            [r#"{"role":"user","providerID":"opencode-go","cost":9.0,"time":{"completed":3600000}}"#],
        )?;
        drop(conn);

        let snapshot = OpencodeGoUsageStore::fetch_usage_from_path_at(
            tmp.path(),
            Utc.timestamp_millis_opt(10 * 60 * 60 * 1000)
                .single()
                .unwrap(),
        )?;

        assert_eq!(snapshot.total_messages, 1);
        let five_hour = snapshot
            .windows
            .iter()
            .find(|window| window.entry_type == "five_hour_spend")
            .ok_or("missing five_hour window")?;
        assert!((five_hour.spent_usd - 0.0).abs() < LIMIT_EPSILON);
        let weekly = snapshot
            .windows
            .iter()
            .find(|window| window.entry_type == "weekly_spend")
            .ok_or("missing weekly window")?;
        assert!((weekly.spent_usd - 1.5).abs() < LIMIT_EPSILON);
        Ok(())
    }
}
